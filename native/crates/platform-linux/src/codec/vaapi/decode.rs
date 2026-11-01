//! VA-API H.264 decoder (task L4): `VAEntrypointVLD`, frames decoded into
//! NV12 surfaces and copied out as `FrameData::Cpu(Nv12)`.
//!
//! VA-API decoding is stateless: the driver decodes slice data, everything
//! else is ours — parsing, POC, the reference buffer and the reference lists
//! (`super::super::h264dec`, unit-tested on real encoder output). Per picture
//! this module turns the plan into `VAPictureParameterBufferH264`, one
//! `VAIQMatrixBufferH264`, and a `VASliceParameterBufferH264` + slice-data
//! buffer per slice, in FFmpeg's submission order, then reads the decoded
//! surface back.
//!
//! * Profiles: Constrained Baseline, Main, High — 8-bit 4:2:0 progressive,
//!   no B-frames (what every host encoder sends; anything else is refused
//!   with [`CodecError::Unsupported`] naming the tool). A Baseline stream is
//!   decoded with the first of Constrained Baseline / Main / High the driver
//!   lists (the Baseline-only tools are refused or skipped upstream: FMO by
//!   the PPS parser, redundant slices by the planner).
//! * Surfaces: `max_num_ref_frames + 1` NV12 surfaces at the coded size —
//!   the references plus the picture being decoded. Nothing is held for
//!   display: the picture is copied out before `decode` returns.
//! * Read-back: `vaDeriveImage` + `vaMapBuffer` (no copy on the GPU side),
//!   falling back for good to `vaGetImage` into an image of our own when the
//!   driver refuses to derive (Mesa/AMD does for tiled surfaces). Either way
//!   the visible (cropped) rectangle is copied with the driver's pitches into
//!   the tightly packed `Nv12Picture`.
//! * A new SPS at an IDR (resolution change) re-creates the surfaces and the
//!   context; [`VideoDecoder::dimensions`] reports the new visible size.
//! * Loss: a picture that would predict from a reference that was never
//!   decoded is not submitted (`Ok(None)`, so the viewer's watchdog asks for
//!   a keyframe) instead of smearing garbage until the next IDR.
//!
//! ## References
//! * libva 2.20 `va.h` (§"H.264 decoding": `VAPictureParameterBufferH264`,
//!   `VAIQMatrixBufferH264`, `VASliceParameterBufferH264`,
//!   `slice_data_bit_offset` semantics), `vaDeriveImage`/`vaGetImage` docs.
//! * FFmpeg `libavcodec/vaapi_h264.c` (field mapping, inferred weights,
//!   `MinLumaBiPredSize8x8`), `vaapi_decode.c` (buffer order),
//!   `hwcontext_vaapi.c` (derive-or-get read-back).
//! * GStreamer `sys/va/gstvah264dec.c` (scaling lists in raster order,
//!   reference-frame flags).
//! * Intel media-driver `media_driver/linux/common/codec/ddi/media_ddi_decode_avc.cpp`
//!   and Mesa `src/gallium/frontends/va/picture_h264.c` (what each field
//!   feeds).

use std::ffi::{c_int, c_void};
use std::path::PathBuf;
use std::rc::Rc;

use platform_api::{Codec, CodecError, Frame, VideoDecoder};

use super::ffi::*;
use super::{
    create_buffer, create_buffer_raw, create_config, create_context, create_surfaces, first_node,
    image_format, max_surface_size, render, Config, Context, Display, Surfaces,
};
use crate::codec::h264dec::params::{PocType, Pps, Sps};
use crate::codec::h264dec::refs::{Dpb, ListEntry, Pixels, RefFrame};
use crate::codec::h264dec::slice::{PredWeights, RefWeights};
use crate::codec::h264dec::stream::{max_refs, Picture, Slice, Stream};
use crate::codec::nv12::{Nv12Picture, SourcePlane, Visible};
use crate::codec::zeroed::Zeroed;

/// The H.264 decode profiles this decoder can use, in the order they are
/// considered for a Baseline stream.
const H264_PROFILES: [VAProfile; 3] = [
    VA_PROFILE_H264_CONSTRAINED_BASELINE,
    VA_PROFILE_H264_MAIN,
    VA_PROFILE_H264_HIGH,
];

fn profile_name(p: VAProfile) -> &'static str {
    match p {
        VA_PROFILE_H264_CONSTRAINED_BASELINE => "Constrained Baseline",
        VA_PROFILE_H264_MAIN => "Main",
        VA_PROFILE_H264_HIGH => "High",
        _ => "other",
    }
}

/// What the decode probe found on the first render node that decodes H.264.
#[derive(Debug, Clone)]
pub(crate) struct DecodeProbe {
    pub node: PathBuf,
    pub vendor: String,
    pub version: (c_int, c_int),
    /// H.264 profiles with `VAEntrypointVLD` and 4:2:0 surfaces.
    pub profiles: Vec<VAProfile>,
}

impl DecodeProbe {
    pub(crate) fn describe(&self) -> String {
        let names: Vec<&str> = self.profiles.iter().map(|&p| profile_name(p)).collect();
        format!(
            "VA-API on {} — {} (VA-API {}.{}; H.264 {})",
            self.node.display(),
            self.vendor,
            self.version.0,
            self.version.1,
            names.join("/")
        )
    }
}

/// Probe every render node; the first with an H.264 decode entrypoint wins.
pub(crate) fn decode_probe() -> Result<DecodeProbe, String> {
    first_node(|va, node| {
        let display = Display::open(va, node)?;
        let listed = display.profiles()?;
        let mut profiles = Vec::new();
        for p in H264_PROFILES.into_iter().filter(|p| listed.contains(p)) {
            if !display.entrypoints(p)?.contains(&VA_ENTRYPOINT_VLD) {
                continue;
            }
            let rt = display.attributes(p, VA_ENTRYPOINT_VLD, &[VA_CONFIG_ATTRIB_RT_FORMAT])?[0];
            if rt != VA_ATTRIB_NOT_SUPPORTED && rt & VA_RT_FORMAT_YUV420 != 0 {
                profiles.push(p);
            }
        }
        if profiles.is_empty() {
            return Err(format!(
                "{} has no H.264 decode entrypoint (`vainfo` must list VAProfileH264Main or \
                 VAProfileH264High : VAEntrypointVLD; install the GPU's VA-API driver — \
                 intel-media-va-driver or mesa-va-drivers)",
                display.vendor()
            ));
        }
        Ok(DecodeProbe {
            node: node.to_path_buf(),
            vendor: display.vendor(),
            version: display.version,
            profiles,
        })
    })
}

/// The VA profile to decode `sps` with, from those the driver offers.
fn pick_profile(sps: &Sps, offered: &[VAProfile]) -> Result<VAProfile, CodecError> {
    let wanted: &[VAProfile] = match sps.profile_idc {
        // Constrained Baseline is a subset of Main and High; plain Baseline's
        // extra tools are refused (FMO) or skipped (redundant slices) before
        // the driver sees them.
        66 => &H264_PROFILES,
        77 => &[VA_PROFILE_H264_MAIN, VA_PROFILE_H264_HIGH],
        100 => &[VA_PROFILE_H264_HIGH],
        other => {
            return Err(CodecError::Unsupported(format!(
                "H.264 profile_idc {other} (only Constrained Baseline, Main and High decode \
                 on VA-API here)"
            )))
        }
    };
    wanted
        .iter()
        .copied()
        .find(|p| offered.contains(p))
        .ok_or_else(|| {
            let names: Vec<&str> = offered.iter().map(|&p| profile_name(p)).collect();
            CodecError::Unsupported(format!(
                "H.264 profile_idc {} — this GPU's VA-API driver decodes only {}",
                sps.profile_idc,
                names.join("/")
            ))
        })
}

/// A `VAImage` of our own (the `vaGetImage` read-back target).
struct Image {
    d: Rc<Display>,
    image: VAImage,
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: created on this display; nothing maps it past `read_back`.
        unsafe { (self.d.va.destroy_image)(self.d.dpy, self.image.image_id) };
    }
}

/// The driver objects for one coded size and profile.
struct Session {
    // Field order is drop order: image and context before the surfaces they
    // use, the config last.
    image: Option<Image>,
    ctx: Context,
    surfaces: Surfaces,
    _config: Config,
    va_profile: VAProfile,
    coded: (u32, u32),
}

pub(crate) struct VaapiDecoder {
    // Dropped before `display` (each holds an `Rc<Display>` anyway).
    session: Option<Session>,
    stream: Stream<VASurfaceID>,
    picture: Nv12Picture,
    /// Last visible size (the hint until the first picture).
    dims: (u32, u32),
    /// `vaDeriveImage` works for this session's surfaces (see the module docs).
    derive: bool,
    probe: DecodeProbe,
    display: Rc<Display>,
}

impl VaapiDecoder {
    pub(crate) fn new(
        probe: &DecodeProbe,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        if codec != Codec::H264 {
            return Err(CodecError::NoDecoder(codec));
        }
        let va = Va::shared().map_err(|e| CodecError::Backend(format!("VA-API: {e}")))?;
        let display = Display::open(&va, &probe.node)
            .map_err(|e| CodecError::Backend(format!("VA-API decoder: {e}")))?;
        tracing::info!("VA-API decoder ready: {}", probe.describe());
        Ok(Self {
            session: None,
            stream: Stream::default(),
            picture: Nv12Picture::default(),
            dims: (width, height),
            derive: true,
            probe: probe.clone(),
            display: Rc::new(display),
        })
    }

    /// Driver objects for the sequence `sps` starts: kept when the profile and
    /// coded size are unchanged and there are enough surfaces, re-created
    /// otherwise (a resolution change arrives with an IDR).
    fn ensure_session(&mut self, sps: &Sps) -> Result<(), CodecError> {
        let va_profile = pick_profile(sps, &self.probe.profiles)?;
        let coded = (sps.coded_width(), sps.coded_height());
        // The references plus the picture being decoded.
        let needed = max_refs(sps) + 1;
        if let Some(s) = &self.session {
            if s.va_profile == va_profile && s.coded == coded && s.surfaces.ids.len() >= needed {
                return Ok(());
            }
        }
        self.session = None;
        let backend = |e: String| CodecError::Backend(format!("VA-API decoder: {e}"));
        let d = &self.display;
        let config = create_config(
            d,
            va_profile,
            VA_ENTRYPOINT_VLD,
            &mut [VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            }],
        )
        .map_err(backend)?;
        let (max_w, max_h) = max_surface_size(d, &config).map_err(backend)?;
        if coded.0 > max_w || coded.1 > max_h {
            return Err(CodecError::Unsupported(format!(
                "a {}x{} H.264 stream — {} decodes up to {max_w}x{max_h}",
                coded.0, coded.1, self.probe.vendor
            )));
        }
        let surfaces = create_surfaces(
            d,
            VA_RT_FORMAT_YUV420,
            VA_FOURCC_NV12,
            coded.0,
            coded.1,
            needed,
        )
        .map_err(backend)?;
        let ctx = create_context(d, &config, coded.0, coded.1, &surfaces.ids).map_err(backend)?;
        let (w, h) = sps.visible_size();
        tracing::info!(
            "VA-API decoder: H.264 {} (profile_idc {}, level {}.{}) {w}x{h} (coded {}x{}), {} surfaces",
            profile_name(va_profile),
            sps.profile_idc,
            sps.level_idc / 10,
            sps.level_idc % 10,
            coded.0,
            coded.1,
            needed
        );
        self.session = Some(Session {
            image: None,
            ctx,
            surfaces,
            _config: config,
            va_profile,
            coded,
        });
        // New surfaces: give deriving another chance.
        self.derive = true;
        Ok(())
    }

    /// A surface no reference frame occupies.
    fn free_surface(&self, session: &Session) -> Result<VASurfaceID, String> {
        let dpb = self.stream.dpb();
        session
            .surfaces
            .ids
            .iter()
            .copied()
            .find(|id| dpb.frames().iter().all(|f| f.surface() != Some(id)))
            .ok_or_else(|| "every surface holds a reference frame".into())
    }

    /// Submit `pic` for decoding into `target`.
    fn submit(
        &self,
        session: &Session,
        pic: &Picture<'_>,
        target: VASurfaceID,
    ) -> Result<(), String> {
        let ctx = &session.ctx;
        let dpb = self.stream.dpb();
        let mut buffers = Vec::with_capacity(2 + 2 * pic.slices.len());
        let pp = picture_parameters(pic, dpb, target);
        buffers.push(create_buffer(ctx, VA_PICTURE_PARAMETER_BUFFER_TYPE, &pp)?);
        let iq = iq_matrix(&pic.sps, &pic.pps);
        buffers.push(create_buffer(ctx, VA_IQ_MATRIX_BUFFER_TYPE, &iq)?);
        for slice in &pic.slices {
            let sp = slice_parameters(slice, dpb)?;
            buffers.push(create_buffer(ctx, VA_SLICE_PARAMETER_BUFFER_TYPE, &sp)?);
            buffers.push(create_buffer_raw(
                ctx,
                VA_SLICE_DATA_BUFFER_TYPE,
                slice.nal.len(),
                slice.nal.as_ptr().cast(),
            )?);
        }
        render(ctx, target, &buffers)?;
        let d = &self.display;
        // SAFETY: surface on this display.
        d.check(
            unsafe { (d.va.sync_surface)(d.dpy, target) },
            "vaSyncSurface (decode)",
        )
    }

    /// Copy the visible part of `surface` into `self.picture`.
    fn read_back(&mut self, surface: VASurfaceID, sps: &Sps) -> Result<(), String> {
        let (width, height) = sps.visible_size();
        let visible = Visible {
            x: sps.crop.left,
            y: sps.crop.top,
            width,
            height,
        };
        let d = self.display.clone();
        if self.derive {
            let mut image = VAImage::zeroed();
            // SAFETY: valid surface; out-struct.
            let status = unsafe { (d.va.derive_image)(d.dpy, surface, &mut image) };
            if status == VA_STATUS_SUCCESS {
                let copied = if image.format.fourcc == VA_FOURCC_NV12 {
                    copy_image(&d, &image, visible, &mut self.picture)
                } else {
                    Err("derived image is not NV12".into())
                };
                // SAFETY: derived above, unmapped by `copy_image`.
                unsafe { (d.va.destroy_image)(d.dpy, image.image_id) };
                match copied {
                    Ok(()) => return Ok(()),
                    Err(e) => tracing::debug!("VA-API: derived image unusable ({e})"),
                }
            }
            tracing::debug!("VA-API: reading decoded pictures back through vaGetImage");
            self.derive = false;
        }
        let session = self
            .session
            .as_mut()
            .ok_or("no decoding session to read back from")?;
        let (cw, ch) = session.coded;
        if session.image.is_none() {
            let mut format = image_format(VA_FOURCC_NV12);
            let mut image = VAImage::zeroed();
            // SAFETY: format and out-struct valid for the call.
            d.check(
                unsafe {
                    (d.va.create_image)(d.dpy, &mut format, cw as c_int, ch as c_int, &mut image)
                },
                "vaCreateImage (read-back)",
            )?;
            session.image = Some(Image {
                d: d.clone(),
                image,
            });
        }
        let image = session
            .image
            .as_ref()
            .map(|i| i.image)
            .ok_or("read-back image missing")?;
        // SAFETY: surface and image on this display; the image is the coded
        // size, so the whole surface fits.
        d.check(
            unsafe { (d.va.get_image)(d.dpy, surface, 0, 0, cw, ch, image.image_id) },
            "vaGetImage",
        )?;
        copy_image(&d, &image, visible, &mut self.picture)
    }
}

/// Map `image`, copy its `visible` rectangle into `out`, unmap.
fn copy_image(
    d: &Display,
    image: &VAImage,
    visible: Visible,
    out: &mut Nv12Picture,
) -> Result<(), String> {
    if image.num_planes < 2 {
        return Err(format!("NV12 image with {} planes", image.num_planes));
    }
    let mut base: *mut c_void = std::ptr::null_mut();
    // SAFETY: the image's buffer belongs to this display.
    d.check(
        unsafe { (d.va.map_buffer)(d.dpy, image.buf, &mut base) },
        "vaMapBuffer (read-back)",
    )?;
    let copied = if base.is_null() {
        Err("vaMapBuffer returned no memory".into())
    } else {
        // SAFETY: libva maps `data_size` bytes at `base` until the unmap
        // below; `copy_from` bounds-checks every row against this slice.
        let mapped =
            unsafe { std::slice::from_raw_parts(base as *const u8, image.data_size as usize) };
        out.copy_from(
            mapped,
            SourcePlane {
                offset: image.offsets[0] as usize,
                pitch: image.pitches[0] as usize,
            },
            SourcePlane {
                offset: image.offsets[1] as usize,
                pitch: image.pitches[1] as usize,
            },
            visible,
        )
    };
    // SAFETY: mapped above.
    let unmapped = d.check(
        unsafe { (d.va.unmap_buffer)(d.dpy, image.buf) },
        "vaUnmapBuffer (read-back)",
    );
    copied.and(unmapped)
}

/// The `VAPictureH264` of a reference frame.
fn va_picture(f: &RefFrame<VASurfaceID>) -> VAPictureH264 {
    let Some(&surface) = f.surface() else {
        return VAPictureH264::INVALID;
    };
    let (frame_idx, flags) = match f.long_term_idx {
        Some(idx) => (idx, VA_PICTURE_H264_LONG_TERM_REFERENCE),
        None => (f.frame_num, VA_PICTURE_H264_SHORT_TERM_REFERENCE),
    };
    VAPictureH264 {
        picture_id: surface,
        frame_idx,
        flags,
        top_field_order_cnt: f.poc[0],
        bottom_field_order_cnt: f.poc[1],
        va_reserved: [0; 4],
    }
}

/// `VAPictureParameterBufferH264` for `pic`, decoded into `target`.
fn picture_parameters(
    pic: &Picture<'_>,
    dpb: &Dpb<VASurfaceID>,
    target: VASurfaceID,
) -> VAPictureParameterBufferH264 {
    let (sps, pps, first) = (&*pic.sps, &*pic.pps, pic.first());
    let poc = pic.poc.as_i32();
    let mut pp = VAPictureParameterBufferH264::zeroed();
    pp.curr_pic = VAPictureH264 {
        picture_id: target,
        frame_idx: first.frame_num,
        // FFmpeg's convention: a reference picture is flagged short-term while
        // it is decoded (marking — which may make it long-term — comes after).
        flags: if pic.is_reference() {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        } else {
            0
        },
        top_field_order_cnt: poc[0],
        bottom_field_order_cnt: poc[1],
        va_reserved: [0; 4],
    };
    pp.reference_frames = [VAPictureH264::INVALID; 16];
    // Frames without pixels (lost / inferred) have no surface to name; the
    // lists never point a decodable picture at them.
    for (slot, f) in pp
        .reference_frames
        .iter_mut()
        .zip(dpb.frames().iter().filter(|f| f.surface().is_some()))
    {
        *slot = va_picture(f);
    }
    // Sizes are bounded by the SPS parser (≤ 1024 macroblocks each way).
    pp.picture_width_in_mbs_minus1 = (sps.width_mbs - 1) as u16;
    pp.picture_height_in_mbs_minus1 = (sps.height_mbs() - 1) as u16;
    pp.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8 as u8;
    pp.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8 as u8;
    pp.num_ref_frames = sps.max_num_ref_frames as u8;
    let (poc_type, log2_lsb_minus4, always_zero) = match &sps.poc {
        PocType::Lsb { log2_max_lsb } => (0, log2_max_lsb - 4, false),
        PocType::Cycle {
            delta_pic_order_always_zero,
            ..
        } => (1, 0, *delta_pic_order_always_zero),
        PocType::FrameNum => (2, 0, false),
    };
    pp.seq_fields = (sps.chroma_format_idc << DEC_SEQ_CHROMA_FORMAT_IDC_SHIFT)
        | flag(sps.gaps_in_frame_num_allowed, DEC_SEQ_GAPS_IN_FRAME_NUM_ALLOWED)
        | flag(sps.frame_mbs_only, DEC_SEQ_FRAME_MBS_ONLY)
        | flag(sps.direct_8x8_inference, DEC_SEQ_DIRECT_8X8_INFERENCE)
        // §A.3.3.2: from level 3.1 on, bi-predicted blocks are at least 8x8.
        | flag(sps.level_idc >= 31, DEC_SEQ_MIN_LUMA_BI_PRED_SIZE_8X8)
        | ((sps.log2_max_frame_num - 4) << DEC_SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT)
        | (poc_type << DEC_SEQ_PIC_ORDER_CNT_TYPE_SHIFT)
        | (log2_lsb_minus4 << DEC_SEQ_LOG2_MAX_POC_LSB_MINUS4_SHIFT)
        | flag(always_zero, DEC_SEQ_DELTA_PIC_ORDER_ALWAYS_ZERO);
    // Ranges are enforced by the PPS parser (−26…25, −12…12), so they fit i8.
    pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
    pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
    pp.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
    pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
    pp.pic_fields = flag(pps.entropy_coding_mode, DEC_PIC_ENTROPY_CODING_MODE)
        | flag(pps.weighted_pred, DEC_PIC_WEIGHTED_PRED)
        | (pps.weighted_bipred_idc << DEC_PIC_WEIGHTED_BIPRED_IDC_SHIFT)
        | flag(pps.transform_8x8_mode, DEC_PIC_TRANSFORM_8X8_MODE)
        | flag(pps.constrained_intra_pred, DEC_PIC_CONSTRAINED_INTRA_PRED)
        | flag(
            pps.bottom_field_pic_order_in_frame_present,
            DEC_PIC_PIC_ORDER_PRESENT,
        )
        | flag(
            pps.deblocking_filter_control_present,
            DEC_PIC_DEBLOCKING_FILTER_CONTROL_PRESENT,
        )
        | flag(
            pps.redundant_pic_cnt_present,
            DEC_PIC_REDUNDANT_PIC_CNT_PRESENT,
        )
        | flag(pic.is_reference(), DEC_PIC_REFERENCE);
    // frame_num < 2^16 (log2_max_frame_num ≤ 16).
    pp.frame_num = first.frame_num as u16;
    pp
}

fn flag(on: bool, bit: u32) -> u32 {
    if on {
        bit
    } else {
        0
    }
}

/// `VAIQMatrixBufferH264`: the picture's resolved scaling matrices (raster
/// order); VA-API carries the two luma 8x8 lists (4:4:4 is refused).
fn iq_matrix(sps: &Sps, pps: &Pps) -> VAIQMatrixBufferH264 {
    let m = pps.scaling(sps);
    let mut iq = VAIQMatrixBufferH264::zeroed();
    iq.scaling_list_4x4 = m.l4x4;
    iq.scaling_list_8x8 = [m.l8x8[0], m.l8x8[1]];
    iq
}

/// One list's weights in VA-API's layout.
struct ListWeights {
    luma_flag: u8,
    luma_weight: [i16; 32],
    luma_offset: [i16; 32],
    chroma_flag: u8,
    chroma_weight: [[i16; 2]; 32],
    chroma_offset: [[i16; 2]; 32],
}

/// Explicit or inferred (§7.4.3.2: weight `2^denom`, offset 0) weights for
/// the first `n` references, and whether any were explicit — VA-API wants
/// the inferred values filled in, not only the transmitted ones.
fn weights(list: &[RefWeights], n: usize, luma_denom: u32, chroma_denom: u32) -> ListWeights {
    let mut w = ListWeights {
        luma_flag: list.iter().any(|r| r.luma.is_some()) as u8,
        luma_weight: [0; 32],
        luma_offset: [0; 32],
        chroma_flag: list.iter().any(|r| r.chroma.is_some()) as u8,
        chroma_weight: [[0; 2]; 32],
        chroma_offset: [[0; 2]; 32],
    };
    for i in 0..n.min(32) {
        let r = list.get(i).copied().unwrap_or_default();
        // Weights/offsets are bounded to −128…127 by the parser.
        (w.luma_weight[i], w.luma_offset[i]) = match r.luma {
            Some((wt, off)) => (wt as i16, off as i16),
            None => (1 << luma_denom, 0),
        };
        for j in 0..2 {
            (w.chroma_weight[i][j], w.chroma_offset[i][j]) = match r.chroma {
                Some(c) => (c[j].0 as i16, c[j].1 as i16),
                None => (1 << chroma_denom, 0),
            };
        }
    }
    w
}

/// `VASliceParameterBufferH264` for one slice.
fn slice_parameters(
    slice: &Slice<'_>,
    dpb: &Dpb<VASurfaceID>,
) -> Result<VASliceParameterBufferH264, String> {
    let h = &slice.header;
    let mut sp = VASliceParameterBufferH264::zeroed();
    sp.slice_data_size =
        u32::try_from(slice.nal.len()).map_err(|_| format!("a {}-byte slice", slice.nal.len()))?;
    sp.slice_data_offset = 0;
    sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
    sp.slice_data_bit_offset = u16::try_from(h.header_bits)
        .map_err(|_| format!("a {}-bit slice header", h.header_bits))?;
    // VA-API's field is 16 bits; only pictures past 65535 macroblocks (beyond
    // 8K) with several slices could exceed it.
    sp.first_mb_in_slice = u16::try_from(h.first_mb_in_slice).map_err(|_| {
        format!(
            "first_mb_in_slice {} does not fit VA-API's 16-bit field",
            h.first_mb_in_slice
        )
    })?;
    sp.slice_type = h.slice_type.raw();
    sp.direct_spatial_mv_pred_flag = h.direct_spatial_mv_pred as u8;
    // Active counts are ≤ 32 (parser); 0 for I slices, which have no list.
    sp.num_ref_idx_l0_active_minus1 = h.num_ref_idx_l0_active.saturating_sub(1) as u8;
    sp.num_ref_idx_l1_active_minus1 = h.num_ref_idx_l1_active.saturating_sub(1) as u8;
    sp.cabac_init_idc = h.cabac_init_idc as u8;
    // Bounded by the parser's QP check (−51…51) and offset ranges (−6…6).
    sp.slice_qp_delta = h.slice_qp_delta as i8;
    sp.disable_deblocking_filter_idc = h.disable_deblocking_filter_idc as u8;
    sp.slice_alpha_c0_offset_div2 = h.slice_alpha_c0_offset_div2 as i8;
    sp.slice_beta_offset_div2 = h.slice_beta_offset_div2 as i8;
    sp.ref_pic_list0 = [VAPictureH264::INVALID; 32];
    sp.ref_pic_list1 = [VAPictureH264::INVALID; 32];
    for (slot, entry) in sp.ref_pic_list0.iter_mut().zip(&slice.list0) {
        if let ListEntry::Ref(i) = *entry {
            *slot = va_picture(&dpb.frames()[i]);
        }
    }
    let default = PredWeights::default();
    let pw = h.pred_weights.as_ref().unwrap_or(&default);
    sp.luma_log2_weight_denom = pw.luma_log2_denom as u8;
    sp.chroma_log2_weight_denom = pw.chroma_log2_denom as u8;
    let l0 = weights(
        &pw.l0,
        h.num_ref_idx_l0_active as usize,
        pw.luma_log2_denom,
        pw.chroma_log2_denom,
    );
    sp.luma_weight_l0_flag = l0.luma_flag;
    sp.luma_weight_l0 = l0.luma_weight;
    sp.luma_offset_l0 = l0.luma_offset;
    sp.chroma_weight_l0_flag = l0.chroma_flag;
    sp.chroma_weight_l0 = l0.chroma_weight;
    sp.chroma_offset_l0 = l0.chroma_offset;
    let l1 = weights(
        &pw.l1,
        h.num_ref_idx_l1_active as usize,
        pw.luma_log2_denom,
        pw.chroma_log2_denom,
    );
    sp.luma_weight_l1_flag = l1.luma_flag;
    sp.luma_weight_l1 = l1.luma_weight;
    sp.luma_offset_l1 = l1.luma_offset;
    sp.chroma_weight_l1_flag = l1.chroma_flag;
    sp.chroma_weight_l1 = l1.chroma_weight;
    sp.chroma_offset_l1 = l1.chroma_offset;
    Ok(sp)
}

impl VideoDecoder for VaapiDecoder {
    /// `keyframe` and `pts_100ns` are not needed: the bitstream says which
    /// pictures are IDRs, and nothing is reordered.
    fn decode(
        &mut self,
        au: &[u8],
        _keyframe: bool,
        _pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        let Some(pic) = self.stream.begin(au).map_err(CodecError::from)? else {
            return Ok(None);
        };
        if pic.idr {
            if let Err(e) = self.ensure_session(&pic.sps) {
                self.stream.finish(&pic, Pixels::Lost);
                return Err(e);
            }
        }
        let Some(session) = self.session.take() else {
            // The sequence's IDR could not get driver objects; its pictures
            // are lost until an IDR succeeds.
            self.stream.finish(&pic, Pixels::Lost);
            return Ok(None);
        };
        if !pic.decodable {
            self.session = Some(session);
            self.stream.finish(&pic, Pixels::Lost);
            return Ok(None);
        }
        let decoded = self
            .free_surface(&session)
            .and_then(|target| self.submit(&session, &pic, target).map(|()| target));
        self.session = Some(session);
        let target = match decoded {
            Ok(t) => t,
            Err(e) => {
                self.stream.finish(&pic, Pixels::Lost);
                return Err(CodecError::Backend(format!("VA-API decode: {e}")));
            }
        };
        // The picture is decoded and may be referenced whatever happens to
        // the copy below.
        self.stream.finish(&pic, Pixels::Decoded(target));
        self.read_back(target, &pic.sps)
            .map_err(|e| CodecError::Backend(format!("VA-API read-back: {e}")))?;
        self.dims = (self.picture.width(), self.picture.height());
        Ok(Some(self.picture.frame()))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.dims
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264dec::stream::tests::au;

    fn sps_with(profile_idc: u8) -> Sps {
        let (mut sps, _) = crate::codec::h264dec::slice::tests::main_sets();
        sps.profile_idc = profile_idc;
        sps
    }

    #[test]
    fn profile_choice() {
        let all = H264_PROFILES;
        assert_eq!(
            pick_profile(&sps_with(66), &all).expect("CB"),
            VA_PROFILE_H264_CONSTRAINED_BASELINE
        );
        // AMD (radeonsi) lists no Constrained Baseline on some versions.
        assert_eq!(
            pick_profile(&sps_with(66), &[VA_PROFILE_H264_MAIN, VA_PROFILE_H264_HIGH])
                .expect("Main"),
            VA_PROFILE_H264_MAIN
        );
        assert_eq!(
            pick_profile(&sps_with(77), &all).expect("Main"),
            VA_PROFILE_H264_MAIN
        );
        assert_eq!(
            pick_profile(&sps_with(77), &[VA_PROFILE_H264_HIGH]).expect("High"),
            VA_PROFILE_H264_HIGH
        );
        assert_eq!(
            pick_profile(&sps_with(100), &all).expect("High"),
            VA_PROFILE_H264_HIGH
        );
        assert!(matches!(
            pick_profile(&sps_with(100), &[VA_PROFILE_H264_MAIN]),
            Err(CodecError::Unsupported(_))
        ));
        assert!(matches!(
            pick_profile(&sps_with(110), &all),
            Err(CodecError::Unsupported(_))
        ));
    }

    fn plan_two_pictures() -> (Stream<VASurfaceID>, Vec<u8>, Vec<u8>) {
        let idr = au(true, 3, true, |w| {
            w.ue(0);
            w.ue(7);
            w.ue(0);
            w.bits(0, 16);
            w.ue(0);
            w.flag(false);
            w.flag(false);
            w.se(2);
            w.ue(0);
            w.se(-1);
            w.se(1);
        });
        let p = au(false, 2, false, |w| {
            w.ue(0);
            w.ue(5);
            w.ue(0);
            w.bits(1, 16);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.ue(2); // cabac_init_idc
            w.se(-4);
            w.ue(1); // deblocking off
        });
        (Stream::default(), idr, p)
    }

    #[test]
    fn picture_and_slice_parameters_follow_the_plan() {
        let (mut stream, idr, p) = plan_two_pictures();
        let pic = stream.begin(&idr).expect("plan").expect("picture");
        let pp = picture_parameters(&pic, stream.dpb(), 40);
        assert_eq!(pp.curr_pic.picture_id, 40);
        assert_eq!(pp.curr_pic.flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
        assert!(pp
            .reference_frames
            .iter()
            .all(|r| r.flags == VA_PICTURE_H264_INVALID));
        // 320x240: 20x15 macroblocks.
        assert_eq!(
            (
                pp.picture_width_in_mbs_minus1,
                pp.picture_height_in_mbs_minus1
            ),
            (19, 14)
        );
        assert_eq!(pp.num_ref_frames, 1);
        // chroma 1, frame_mbs_only, direct_8x8, level 4.x ≥ 3.1, log2 16−4,
        // POC type 2.
        assert_eq!(
            pp.seq_fields,
            1 | DEC_SEQ_FRAME_MBS_ONLY
                | DEC_SEQ_DIRECT_8X8_INFERENCE
                | DEC_SEQ_MIN_LUMA_BI_PRED_SIZE_8X8
                | (12 << DEC_SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT)
                | (2 << DEC_SEQ_PIC_ORDER_CNT_TYPE_SHIFT)
        );
        assert_eq!(
            pp.pic_fields,
            DEC_PIC_ENTROPY_CODING_MODE
                | DEC_PIC_DEBLOCKING_FILTER_CONTROL_PRESENT
                | DEC_PIC_REFERENCE
        );
        let sp = slice_parameters(&pic.slices[0], stream.dpb()).expect("slice params");
        assert_eq!(sp.slice_data_size as usize, pic.slices[0].nal.len());
        assert_eq!(
            sp.slice_data_bit_offset as usize,
            pic.slices[0].header.header_bits
        );
        assert_eq!(sp.slice_type, 2);
        assert_eq!(
            (
                sp.slice_qp_delta,
                sp.slice_alpha_c0_offset_div2,
                sp.slice_beta_offset_div2
            ),
            (2, -1, 1)
        );
        assert!(sp
            .ref_pic_list0
            .iter()
            .all(|r| r.flags == VA_PICTURE_H264_INVALID));
        stream.finish(&pic, Pixels::Decoded(40));

        let pic = stream.begin(&p).expect("plan").expect("picture");
        let pp = picture_parameters(&pic, stream.dpb(), 41);
        assert_eq!(pp.reference_frames[0].picture_id, 40);
        assert_eq!(
            pp.reference_frames[0].flags,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        );
        assert_eq!(pp.reference_frames[1].flags, VA_PICTURE_H264_INVALID);
        assert_eq!((pp.curr_pic.top_field_order_cnt, pp.frame_num), (2, 1));
        let sp = slice_parameters(&pic.slices[0], stream.dpb()).expect("slice params");
        assert_eq!(sp.slice_type, 0);
        assert_eq!(sp.num_ref_idx_l0_active_minus1, 0);
        assert_eq!(sp.ref_pic_list0[0].picture_id, 40);
        assert_eq!(sp.ref_pic_list0[1].flags, VA_PICTURE_H264_INVALID);
        assert_eq!(
            (
                sp.cabac_init_idc,
                sp.slice_qp_delta,
                sp.disable_deblocking_filter_idc
            ),
            (2, -4, 1)
        );
        // No weight table: inferred weights for the active reference only.
        assert_eq!(
            (
                sp.luma_weight_l0_flag,
                sp.luma_weight_l0[0],
                sp.luma_weight_l0[1]
            ),
            (0, 1, 0)
        );
        assert_eq!(sp.chroma_weight_l0[0], [1, 1]);
    }

    #[test]
    fn explicit_and_inferred_weights() {
        let list = [
            RefWeights {
                luma: Some((40, -3)),
                chroma: None,
            },
            RefWeights::default(),
        ];
        let w = weights(&list, 3, 5, 2);
        assert_eq!((w.luma_flag, w.chroma_flag), (1, 0));
        assert_eq!(&w.luma_weight[..4], &[40, 32, 32, 0]);
        assert_eq!(&w.luma_offset[..3], &[-3, 0, 0]);
        assert_eq!(w.chroma_weight[2], [4, 4]);
        assert_eq!(w.chroma_offset[0], [0, 0]);
    }

    #[test]
    fn long_term_reference_pictures_use_their_index() {
        let f = RefFrame {
            frame_num: 9,
            long_term_idx: Some(2),
            poc: [18, 18],
            pixels: Pixels::Decoded(7),
        };
        let v = va_picture(&f);
        assert_eq!(
            (v.picture_id, v.frame_idx, v.flags),
            (7, 2, VA_PICTURE_H264_LONG_TERM_REFERENCE)
        );
        let lost = RefFrame {
            pixels: Pixels::Lost,
            ..f
        };
        assert_eq!(va_picture(&lost).flags, VA_PICTURE_H264_INVALID);
    }

    /// Everything but the driver call, on every committed H.264 golden stream
    /// (NVENC, OpenH264, x264 with High-profile tools): each access unit
    /// plans to exactly one decodable picture whose visible size is FFmpeg's,
    /// and every picture and slice maps to VA-API parameters — with surfaces
    /// recycled the way `VaapiDecoder` does.
    #[test]
    fn golden_streams_plan_and_map_without_a_device() {
        let goldens = crate::codec::golden::all();
        let h264: Vec<_> = goldens.iter().filter(|g| g.codec == Codec::H264).collect();
        assert!(!h264.is_empty());
        for g in h264 {
            let mut stream: Stream<VASurfaceID> = Stream::default();
            let mut pictures = 0;
            for (i, (au, _)) in g.access_units().enumerate() {
                let pic = stream
                    .begin(au)
                    .unwrap_or_else(|e| panic!("{} AU {i}: {e:?}", g.name))
                    .unwrap_or_else(|| panic!("{} AU {i}: no picture", g.name));
                assert!(pic.decodable, "{} AU {i}: undecodable", g.name);
                assert_eq!(
                    pic.sps.visible_size(),
                    g.frames[i].0,
                    "{} AU {i}: visible size",
                    g.name
                );
                let pool = (max_refs(&pic.sps) + 1) as VASurfaceID;
                let target = (0..pool)
                    .find(|id| {
                        stream
                            .dpb()
                            .frames()
                            .iter()
                            .all(|f| f.surface() != Some(id))
                    })
                    .unwrap_or_else(|| panic!("{} AU {i}: no free surface", g.name));
                let pp = picture_parameters(&pic, stream.dpb(), target);
                let refs = pp
                    .reference_frames
                    .iter()
                    .filter(|r| r.flags != VA_PICTURE_H264_INVALID)
                    .count();
                assert_eq!(refs, stream.dpb().frames().len(), "{} AU {i}", g.name);
                for slice in &pic.slices {
                    let sp = slice_parameters(slice, stream.dpb()).expect("slice parameters");
                    // Every reference the list names is a real surface; the
                    // rest of the 32 slots are invalid.
                    for (e, r) in slice.list0.iter().zip(&sp.ref_pic_list0) {
                        match e {
                            ListEntry::Ref(_) => assert!(
                                r.picture_id < pool,
                                "{} AU {i}: reference surface {:#x}",
                                g.name,
                                r.picture_id
                            ),
                            _ => assert_eq!(r.flags, VA_PICTURE_H264_INVALID),
                        }
                    }
                    assert!(sp.ref_pic_list0[slice.list0.len()..]
                        .iter()
                        .all(|r| r.flags == VA_PICTURE_H264_INVALID));
                }
                let _ = iq_matrix(&pic.sps, &pic.pps);
                stream.finish(&pic, Pixels::Decoded(target));
                pictures += 1;
            }
            assert_eq!(pictures, g.frames.len(), "{}", g.name);
        }
    }

    /// Real decode on an Intel/AMD GPU of the committed golden streams, each
    /// picture compared with FFmpeg's decode of the same stream (H.264
    /// decoding is bit-exact, so they must be identical). Run on the Linux
    /// test machine — see plans/checklists/L4.md.
    #[test]
    #[ignore = "needs VA-API device"]
    fn vaapi_decodes_the_golden_streams() {
        let probe = decode_probe().expect("VA-API decode probe");
        eprintln!("VA-API decode probe: {}", probe.describe());
        crate::codec::golden::check_decoder(
            |codec| codec == Codec::H264,
            |codec, w, h| {
                VaapiDecoder::new(&probe, codec, w, h).map(|d| Box::new(d) as Box<dyn VideoDecoder>)
            },
        );
    }
}
