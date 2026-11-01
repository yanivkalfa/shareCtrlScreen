//! NVIDIA NVDEC decoder (feature `nvdec`), bound at run time.
//!
//! `libnvcuvid.so.1` and `libcuda.so.1` ship with the proprietary driver and
//! are `dlopen`ed — nothing is linked and no SDK is needed to build (decision
//! #5). Missing libraries, an old driver or no NVIDIA GPU surface as a probe
//! failure with the reason, and the factory moves on to VA-API.
//!
//! Unlike VA-API, NVDEC comes with the driver's own bitstream parser
//! (`cuvidCreateVideoParser`), which handles parameter sets, POC and the
//! reference buffer for H.264, HEVC and AV1 alike; the decoder feeds it one
//! access unit per call and reacts to its three callbacks:
//! * sequence — (re)create the hardware decoder for the stream's coded size,
//!   cropped to the display area and scaled 1:1 (`ulTargetWidth/Height`).
//!   A mid-stream resolution change arrives here too, and
//!   [`VideoDecoder::dimensions`] follows it.
//! * decode — `cuvidDecodePicture` with the parser's picture parameters.
//! * display — remember which surface to show. `ulMaxDisplayDelay = 0` and
//!   `CUVID_PKT_ENDOFPICTURE` make it fire inside the same parse call for a
//!   stream without reordering, so each access unit yields its picture
//!   immediately.
//!
//! The picture is then mapped (`cuvidMapVideoFrame64`, NV12, pitched) and
//! copied into host memory with `cuMemcpy2D` (v1; the zero-copy seam is the
//! presenter importing the CUDA allocation instead).
//!
//! ## References
//! * NVIDIA Video Codec SDK 12.0 — *NVDEC Video Decoder API Programming
//!   Guide* (§"Video decoder capabilities", §"Creating a decoder", §"Decoding
//!   the frame/field", §"Preparing the decoded frame for further
//!   processing", §"Reconfiguring the decoder": parser callbacks, return
//!   value of the sequence callback, NV12 chroma at `pitch × ((height + 1) &
//!   ~1)`).
//! * `dynlink_cuviddec.h` / `dynlink_nvcuvid.h` / `dynlink_loader.h`, FFmpeg
//!   nv-codec-headers n12.0.16.1 (MIT).
//! * NVIDIA `Samples/NvCodec/NvDecoder/NvDecoder.cpp` (callback handling,
//!   display area → target size) and FFmpeg `libavcodec/cuviddec.c`.

mod ffi;

use std::ffi::{c_int, c_ulong, c_void};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use platform_api::{Codec, CodecError, Frame, VideoDecoder};

use self::ffi::*;
use super::annexb;
use super::cuda::{Context, CuDevicePtr, Cuda, Current};
use super::nv12::Nv12Picture;
use super::zeroed::Zeroed;

/// GPU used for decoding: the first CUDA device, the same one NVENC uses —
/// on laptops and single-GPU desktops the one driving the display.
const CUDA_DEVICE: c_int = 0;

/// What one NVDEC engine accepts for a codec (8-bit 4:2:0, NV12 out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_mb_count: u32,
    pub min_width: u32,
    pub min_height: u32,
}

impl Limits {
    /// Why a `width`×`height` coded picture does not fit, if it does not.
    fn refuse(&self, width: u32, height: u32) -> Option<String> {
        let mbs = width.div_ceil(16) * height.div_ceil(16);
        if width > self.max_width || height > self.max_height || mbs > self.max_mb_count {
            Some(format!(
                "{width}x{height} exceeds the GPU's {}x{} ({} macroblocks)",
                self.max_width, self.max_height, self.max_mb_count
            ))
        } else if width < self.min_width || height < self.min_height {
            Some(format!(
                "{width}x{height} is below the GPU's minimum {}x{}",
                self.min_width, self.min_height
            ))
        } else {
            None
        }
    }
}

/// What the probe found.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    pub device: String,
    pub h264: Option<Limits>,
    pub hevc: Option<Limits>,
    pub av1: Option<Limits>,
}

impl Probe {
    pub(crate) fn limits(&self, codec: Codec) -> Option<Limits> {
        match codec {
            Codec::H264 => self.h264,
            Codec::Hevc => self.hevc,
            Codec::Av1 => self.av1,
        }
    }

    pub(crate) fn describe(&self) -> String {
        let list: Vec<&str> = [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|&c| self.limits(c).is_some())
            .map(Codec::as_caps_str)
            .collect();
        format!("NVDEC on {} ({})", self.device, list.join(", "))
    }
}

fn cuvid_codec(codec: Codec) -> c_int {
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::Hevc => CODEC_HEVC,
        Codec::Av1 => CODEC_AV1,
    }
}

/// One full NVDEC capability probe (driver libs, GPU, decoder caps).
pub(crate) fn probe() -> Result<Probe, String> {
    let api = Cuvid::shared()?;
    let cuda = Cuda::shared()?;
    let ctx = Context::new(&cuda, CUDA_DEVICE)?;
    let _current = ctx.current()?;
    let caps = |codec: Codec| -> Result<Option<Limits>, String> {
        let mut c = CuvidDecodeCaps::zeroed();
        c.codec_type = cuvid_codec(codec);
        c.chroma_format = CHROMA_420;
        c.bit_depth_minus8 = 0;
        // SAFETY: zeroed caps with the IN fields set; the context is current.
        cuda.check(
            unsafe { (api.get_decoder_caps)(&mut c) },
            "cuvidGetDecoderCaps",
        )?;
        let nv12 = c.output_format_mask & (1 << SURFACE_NV12) != 0;
        Ok((c.is_supported != 0 && nv12).then_some(Limits {
            max_width: c.max_width,
            max_height: c.max_height,
            max_mb_count: c.max_mb_count,
            min_width: c.min_width as u32,
            min_height: c.min_height as u32,
        }))
    };
    let probe = Probe {
        device: ctx.device_name().to_string(),
        h264: caps(Codec::H264)?,
        hevc: caps(Codec::Hevc)?,
        av1: caps(Codec::Av1)?,
    };
    if probe.h264.is_none() && probe.hevc.is_none() && probe.av1.is_none() {
        return Err(format!(
            "{} has no 8-bit 4:2:0 H.264/HEVC/AV1 decoder",
            probe.device
        ));
    }
    Ok(probe)
}

/// A hardware decoder for one coded size; destroyed on drop (with the CUDA
/// context current — the parser callbacks and `NvdecDecoder::drop` ensure it).
struct HwDecoder {
    api: Arc<Cuvid>,
    handle: CuvidDecoder,
    coded: (u32, u32),
    display: Rect16,
    surfaces: u32,
    /// Output (`ulTargetWidth/Height`): the display area, rounded up to even.
    target: (u32, u32),
    /// The display area's size, what the caller sees.
    visible: (u32, u32),
}

impl Drop for HwDecoder {
    fn drop(&mut self) {
        // SAFETY: we own the decoder; its frames are unmapped (decode unmaps
        // before returning) and the context is current.
        unsafe { (self.api.destroy_decoder)(self.handle) };
    }
}

/// The surface the display callback chose.
#[derive(Debug, Clone, Copy)]
struct Ready {
    index: c_int,
    progressive: bool,
    top_field_first: bool,
}

/// What the parser callbacks share with `decode`. Reached only through the
/// raw pointer the parser was given (see `NvdecDecoder::state`).
struct State {
    api: Arc<Cuvid>,
    cuda: Arc<Cuda>,
    codec: Codec,
    limits: Limits,
    decoder: Option<HwDecoder>,
    ready: Option<Ready>,
    /// The first failure inside a callback (the parser only sees "0").
    error: Option<CodecError>,
}

fn unsupported(what: String) -> CodecError {
    CodecError::Unsupported(format!("NVDEC: {what}"))
}

fn backend(what: String) -> CodecError {
    CodecError::Backend(format!("NVDEC: {what}"))
}

/// The display area as `CUVIDDECODECREATEINFO` wants it, if it is a
/// non-empty rectangle inside the `coded` picture.
fn display_rect(a: Rect32, coded: (u32, u32)) -> Option<Rect16> {
    let r = Rect16 {
        left: i16::try_from(a.left).ok()?,
        top: i16::try_from(a.top).ok()?,
        right: i16::try_from(a.right).ok()?,
        bottom: i16::try_from(a.bottom).ok()?,
    };
    let inside = r.left >= 0
        && r.top >= 0
        && r.right > r.left
        && r.bottom > r.top
        && r.right as u32 <= coded.0
        && r.bottom as u32 <= coded.1;
    inside.then_some(r)
}

impl State {
    /// The sequence callback: make sure a decoder fits `fmt`; returns the
    /// decode-surface count, which the parser adopts as its DPB size.
    fn sequence(&mut self, fmt: &CuvideoFormat) -> Result<u32, CodecError> {
        if fmt.chroma_format != CHROMA_420 {
            return Err(unsupported(format!(
                "chroma format {} (only 4:2:0 decodes to NV12)",
                fmt.chroma_format
            )));
        }
        if fmt.bit_depth_luma_minus8 != 0 || fmt.bit_depth_chroma_minus8 != 0 {
            return Err(unsupported(format!(
                "{}-bit video (only 8-bit decodes to NV12)",
                fmt.bit_depth_luma_minus8 + 8
            )));
        }
        let coded = (fmt.coded_width, fmt.coded_height);
        if let Some(why) = self.limits.refuse(coded.0, coded.1) {
            return Err(unsupported(why));
        }
        let a = fmt.display_area;
        let rect = display_rect(a, coded).ok_or_else(|| {
            backend(format!(
                "display area {a:?} outside the {}x{} picture",
                coded.0, coded.1
            ))
        })?;
        let visible = (
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        );
        // The guide's minimum; the parser needs this many for its DPB.
        let surfaces = u32::from(fmt.min_num_decode_surfaces).max(1);
        if let Some(d) = &self.decoder {
            if d.coded == coded && d.display == rect && d.surfaces >= surfaces {
                return Ok(d.surfaces);
            }
        }
        // A new sequence at another size: the old decoder's references are
        // gone with it (the parser has flushed them at the sequence change).
        self.decoder = None;
        let target = (visible.0.next_multiple_of(2), visible.1.next_multiple_of(2));
        let mut info = CuvidDecodeCreateInfo::zeroed();
        info.width = coded.0 as c_ulong;
        info.height = coded.1 as c_ulong;
        info.num_decode_surfaces = surfaces as c_ulong;
        info.codec_type = cuvid_codec(self.codec);
        info.chroma_format = CHROMA_420;
        info.creation_flags = CREATE_PREFER_CUVID;
        info.max_width = coded.0 as c_ulong;
        info.max_height = coded.1 as c_ulong;
        info.display_area = rect;
        info.output_format = SURFACE_NV12;
        info.deinterlace_mode = if fmt.progressive_sequence != 0 {
            DEINTERLACE_WEAVE
        } else {
            DEINTERLACE_ADAPTIVE
        };
        info.target_width = target.0 as c_ulong;
        info.target_height = target.1 as c_ulong;
        // One picture is mapped at a time (unmapped before `decode` returns).
        info.num_output_surfaces = 1;
        let mut handle: CuvidDecoder = std::ptr::null_mut();
        // SAFETY: zeroed create-info with every IN field set; out-pointer to
        // a local; the context is current (parse runs under `Current`).
        self.cuda
            .check(
                unsafe { (self.api.create_decoder)(&mut handle, &mut info) },
                "cuvidCreateDecoder",
            )
            .map_err(backend)?;
        tracing::info!(
            "NVDEC decoder: {} {}x{} (coded {}x{}), {} surfaces",
            self.codec.as_caps_str(),
            visible.0,
            visible.1,
            coded.0,
            coded.1,
            surfaces
        );
        self.decoder = Some(HwDecoder {
            api: self.api.clone(),
            handle,
            coded,
            display: rect,
            surfaces,
            target,
            visible,
        });
        Ok(surfaces)
    }

    fn decode(&mut self, params: *mut c_void) -> Result<(), CodecError> {
        let d = self
            .decoder
            .as_ref()
            .ok_or_else(|| backend("picture before any sequence header".into()))?;
        // SAFETY: `params` is the parser's CUVIDPICPARAMS for this call; the
        // context is current.
        self.cuda
            .check(
                unsafe { (self.api.decode_picture)(d.handle, params) },
                "cuvidDecodePicture",
            )
            .map_err(backend)
    }
}

/// Run `f` on the state behind a callback's user pointer, turning a failure
/// into the parser's "0" (and remembering why).
///
/// # Safety
/// `user` is the `State` pointer the parser was created with, and no Rust
/// reference to it is alive (the parser only calls back from inside
/// `cuvidParseVideoData`, during which `decode` holds none).
unsafe fn with_state(
    user: *mut c_void,
    f: impl FnOnce(&mut State) -> Result<c_int, CodecError>,
) -> c_int {
    // SAFETY: guaranteed by the caller.
    let state = unsafe { &mut *(user as *mut State) };
    match f(state) {
        Ok(v) => v,
        Err(e) => {
            state.error.get_or_insert(e);
            0
        }
    }
}

unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut CuvideoFormat) -> c_int {
    // SAFETY: called by the parser with our user pointer and a format valid
    // for the call.
    unsafe {
        with_state(user, |s| {
            let fmt = fmt
                .as_ref()
                .ok_or_else(|| backend("sequence callback without a format".into()))?;
            // Surface counts are at most a few dozen; c_int cannot overflow.
            s.sequence(fmt).map(|n| n as c_int)
        })
    }
}

unsafe extern "C" fn on_decode(user: *mut c_void, params: *mut c_void) -> c_int {
    // SAFETY: as above.
    unsafe { with_state(user, |s| s.decode(params).map(|()| 1)) }
}

unsafe extern "C" fn on_display(user: *mut c_void, info: *mut CuvidParserDispInfo) -> c_int {
    // SAFETY: as above; a null `info` is the end-of-stream notice.
    unsafe {
        with_state(user, |s| {
            if let Some(i) = info.as_ref() {
                s.ready = Some(Ready {
                    index: i.picture_index,
                    progressive: i.progressive_frame != 0,
                    top_field_first: i.top_field_first != 0,
                });
            }
            Ok(1)
        })
    }
}

/// The video parser, destroyed on drop.
struct Parser {
    api: Arc<Cuvid>,
    handle: CuvidParser,
}

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: we own the parser; it calls back only from inside
        // cuvidParseVideoData, which is not running.
        unsafe { (self.api.destroy_video_parser)(self.handle) };
    }
}

pub(crate) struct NvdecDecoder {
    // Torn down explicitly in `Drop` (parser, then the state with its
    // decoder, both with the context current), then the context.
    parser: Option<Parser>,
    /// Owned (`Box::into_raw`); every access goes through this pointer so the
    /// parser's copy stays valid (no `&mut Box` ever invalidates it).
    state: NonNull<State>,
    picture: Nv12Picture,
    dims: (u32, u32),
    have_keyframe: bool,
    codec: Codec,
    cuda: Arc<Cuda>,
    ctx: Rc<Context>,
}

impl NvdecDecoder {
    pub(crate) fn new(
        probe: &Probe,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        let limits = probe.limits(codec).ok_or(CodecError::NoDecoder(codec))?;
        let api = Cuvid::shared().map_err(backend)?;
        let cuda = Cuda::shared().map_err(backend)?;
        let ctx = Rc::new(Context::new(&cuda, CUDA_DEVICE).map_err(backend)?);
        let state = NonNull::from(Box::leak(Box::new(State {
            api: api.clone(),
            cuda: cuda.clone(),
            codec,
            limits,
            decoder: None,
            ready: None,
            error: None,
        })));
        // From here on `Drop` reclaims the state, whatever fails below.
        let mut this = Self {
            parser: None,
            state,
            picture: Nv12Picture::default(),
            dims: (width, height),
            have_keyframe: false,
            codec,
            cuda,
            ctx,
        };
        let mut params = CuvidParserParams::zeroed();
        params.codec_type = cuvid_codec(codec);
        // Raised by the sequence callback's return value.
        params.max_num_decode_surfaces = 1;
        // 0 = 10 MHz, the contract's 100-ns timestamps.
        params.clock_rate = 0;
        params.max_display_delay = 0;
        params.user_data = state.as_ptr().cast();
        params.sequence_callback = Some(on_sequence);
        params.decode_picture = Some(on_decode);
        params.display_picture = Some(on_display);
        let mut handle: CuvidParser = std::ptr::null_mut();
        {
            let _current = this.ctx.current().map_err(backend)?;
            // SAFETY: zeroed params with the callbacks and the state pointer,
            // which lives until `Drop` destroys the parser first.
            this.cuda
                .check(
                    unsafe { (api.create_video_parser)(&mut handle, &mut params) },
                    "cuvidCreateVideoParser",
                )
                .map_err(backend)?;
        }
        this.parser = Some(Parser { api, handle });
        tracing::info!("NVDEC decoder ready: {}", probe.describe());
        Ok(this)
    }

    /// The callback state, while no parse is running.
    fn state(&mut self) -> &mut State {
        // SAFETY: owned by `self`; callbacks run only inside
        // `cuvidParseVideoData`, which never overlaps this borrow.
        unsafe { self.state.as_mut() }
    }

    /// Map surface `ready`, copy its visible area into `self.picture`, unmap.
    fn read_back(&mut self, current: &Current<'_>, ready: Ready) -> Result<(), String> {
        let state = self.state();
        let api = state.api.clone();
        let d = state.decoder.as_ref().ok_or("picture without a decoder")?;
        let (handle, target, visible) = (d.handle, d.target, d.visible);
        let mut status = CuvidGetDecodeStatus::zeroed();
        // SAFETY: valid decoder and index from the display callback.
        let queried = unsafe { (api.get_decode_status)(handle, ready.index, &mut status) };
        // Not every driver/codec reports status; only a definite error counts.
        if queried == 0 {
            match status.decode_status {
                DECODE_STATUS_ERROR => return Err("NVDEC reports a decode error".into()),
                DECODE_STATUS_ERROR_CONCEALED => {
                    tracing::debug!("NVDEC concealed a decode error in this picture")
                }
                _ => {}
            }
        }
        let mut proc = CuvidProcParams::zeroed();
        proc.progressive_frame = ready.progressive as c_int;
        proc.top_field_first = ready.top_field_first as c_int;
        let mut ptr: CuDevicePtr = 0;
        let mut pitch = 0u32;
        // SAFETY: valid decoder/index; out-pointers to locals; context current.
        self.cuda.check(
            unsafe { (api.map_video_frame)(handle, ready.index, &mut ptr, &mut pitch, &mut proc) },
            "cuvidMapVideoFrame",
        )?;
        let copied = self.copy_mapped(current, ptr, pitch as usize, target, visible);
        // SAFETY: mapped above; unmapped whatever the copy did.
        let unmapped = self.cuda.check(
            unsafe { (api.unmap_video_frame)(handle, ptr) },
            "cuvidUnmapVideoFrame",
        );
        copied.and(unmapped)
    }

    /// Copy a mapped NV12 output surface (`target` sized, `pitch` apart) into
    /// the picture, keeping the `visible` part.
    fn copy_mapped(
        &mut self,
        current: &Current<'_>,
        ptr: CuDevicePtr,
        pitch: usize,
        target: (u32, u32),
        visible: (u32, u32),
    ) -> Result<(), String> {
        self.picture.resize(visible.0, visible.1);
        let (y_stride, uv_stride, uv_rows) = (
            self.picture.y_stride(),
            self.picture.uv_stride(),
            self.picture.uv_rows(),
        );
        // NVDEC's NV12 chroma starts after the luma rows of the (even)
        // surface height (Programming Guide, "Preparing the decoded frame").
        let uv_offset = pitch as u64 * u64::from(target.1.next_multiple_of(2));
        let (y, uv) = self.picture.planes_mut();
        current.copy_to_host(ptr, pitch, y, y_stride, y_stride, visible.1 as usize)?;
        current.copy_to_host(ptr + uv_offset, pitch, uv, uv_stride, uv_stride, uv_rows)
    }
}

impl VideoDecoder for NvdecDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        // Deltas before the first keyframe reference pictures this decoder
        // never saw; the parser would conceal them into garbage.
        let key = keyframe || annexb::is_idr(self.codec, au);
        if !key && !self.have_keyframe {
            return Ok(None);
        }
        let parser = self
            .parser
            .as_ref()
            .map(|p| p.handle)
            .ok_or_else(|| backend("parser missing".into()))?;
        let api = {
            let state = self.state();
            state.ready = None;
            state.error = None;
            state.api.clone()
        };
        let mut packet = CuvidSourceDataPacket {
            flags: PKT_ENDOFPICTURE | PKT_TIMESTAMP,
            payload_size: au.len() as c_ulong,
            payload: au.as_ptr(),
            timestamp: pts_100ns,
        };
        // The parser's callbacks and the read-back all need the context.
        let ctx = self.ctx.clone();
        let current = ctx.current().map_err(backend)?;
        // SAFETY: valid parser; `au` outlives the call; no reference to the
        // state is alive while the callbacks use it.
        let parsed = unsafe { (api.parse_video_data)(parser, &mut packet) };
        if let Some(e) = self.state().error.take() {
            return Err(e);
        }
        self.cuda
            .check(parsed, "cuvidParseVideoData")
            .map_err(backend)?;
        if key {
            self.have_keyframe = true;
        }
        let Some(ready) = self.state().ready.take() else {
            return Ok(None);
        };
        self.read_back(&current, ready).map_err(backend)?;
        drop(current);
        self.dims = (self.picture.width(), self.picture.height());
        Ok(Some(self.picture.frame()))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.dims
    }
}

impl Drop for NvdecDecoder {
    fn drop(&mut self) {
        // Destroying the parser and the hardware decoder wants the context
        // current; the context itself goes last (field drop order).
        let _current = self.ctx.current();
        self.parser = None;
        // SAFETY: allocated by `Box::leak` in `new`; the parser that held the
        // pointer is destroyed above, so this is the last use.
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_refuse_with_a_reason() {
        let l = Limits {
            max_width: 4096,
            max_height: 4096,
            max_mb_count: 65536,
            min_width: 48,
            min_height: 16,
        };
        assert_eq!(l.refuse(1920, 1088), None);
        assert!(l.refuse(4112, 64).is_some());
        assert!(l.refuse(32, 32).is_some());
        // 4096x4096 is 65536 macroblocks: fits; one more row does not.
        assert_eq!(l.refuse(4096, 4096), None);
        let tight = Limits {
            // 1920x1088 is 120 x 68 = 8160 macroblocks: one too many.
            max_mb_count: 8159,
            ..l
        };
        assert!(tight.refuse(1920, 1088).is_some());
    }

    /// Real NVDEC decode of the committed golden streams (NVENC and OpenH264
    /// output), each frame compared with FFmpeg's decode of the same stream.
    /// Gated on the driver library: without `libnvcuvid.so.1` (CI) it reports
    /// the skip and passes.
    #[test]
    fn nvdec_decodes_the_golden_streams() {
        let probe = match probe() {
            Ok(p) => p,
            Err(e) if e.contains("not loadable") => {
                eprintln!("skipping NVDEC golden decode: {e}");
                return;
            }
            Err(e) => panic!("NVDEC libraries load but the probe failed: {e}"),
        };
        eprintln!("NVDEC probe: {}", probe.describe());
        crate::codec::golden::check_decoder(
            |codec| probe.limits(codec).is_some(),
            |codec, w, h| {
                NvdecDecoder::new(&probe, codec, w, h).map(|d| Box::new(d) as Box<dyn VideoDecoder>)
            },
        );
    }
}
