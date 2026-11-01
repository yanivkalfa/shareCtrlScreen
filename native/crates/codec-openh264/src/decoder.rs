//! `VideoDecoder` over Cisco OpenH264, producing CPU NV12 frames.

use openh264::decoder::{DecodeOptions, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use platform_api::{Codec, CodecError, Frame, VideoDecoder};

use crate::binary::OpenH264;
use crate::convert;

pub struct SoftwareDecoder {
    inner: Decoder,
    /// Luma of the last picture. A copy, because the wrapper lends the
    /// decoder's planes only for the lifetime of its `DecodedYUV`, shorter than
    /// the contract's "valid until the next `decode`".
    y: Vec<u8>,
    /// Interleaved chroma of the last picture (OpenH264 outputs I420; the
    /// contract's CPU format is NV12).
    uv: Vec<u8>,
    dims: (u32, u32),
    /// Deltas before the first IDR reference pictures this decoder never saw;
    /// they are dropped (`Ok(None)`) instead of being fed in as errors.
    have_keyframe: bool,
}

impl SoftwareDecoder {
    pub fn new(
        openh264: &OpenH264,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        if codec != Codec::H264 {
            return Err(CodecError::NoDecoder(codec));
        }
        let api = openh264
            .load()
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        // Flush after every access unit that returned no picture. OpenH264
        // keeps Main/High-profile pictures in its reorder buffer even from
        // `DecodeFrameNoDelay` (only Constrained Baseline, which cannot
        // reorder, comes out directly), so without the flush a Main stream —
        // what NVENC, Media Foundation and VideoToolbox hosts send — gave no
        // picture at all (found by the I5 golden matrix). The flush is
        // exact here because no encoder in the project reorders (the
        // `platform_api::VideoDecoder` contract): the one buffered picture
        // is the access unit just decoded. The wrapper flushes only when a
        // picture is buffered (`GetOption(DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER)`).
        let config = DecoderConfig::new().flush_after_decode(Flush::Flush);
        let inner = Decoder::with_api_config(api, config)
            .map_err(|e| CodecError::Backend(format!("OpenH264 decoder: {e}")))?;
        tracing::info!("openh264: decoder ready");
        Ok(Self {
            inner,
            y: Vec::new(),
            uv: Vec::new(),
            dims: (width, height),
            have_keyframe: false,
        })
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        _pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        if !keyframe && !self.have_keyframe {
            return Ok(None);
        }
        let decoded = self
            .inner
            .decode_with_options(au, DecodeOptions::new())
            .map_err(|e| CodecError::Backend(format!("OpenH264 decode: {e}")))?;
        let Some(yuv) = decoded else {
            return Ok(None);
        };
        if keyframe {
            self.have_keyframe = true;
        }
        let (w, h) = yuv.dimensions();
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        convert::copy_plane(yuv.y(), y_stride, w, h, &mut self.y);
        convert::interleave_uv(yuv.u(), u_stride, yuv.v(), v_stride, cw, ch, &mut self.uv);
        self.dims = (w as u32, h as u32);
        Ok(Some(Frame::nv12(
            w as u32,
            h as u32,
            &self.y,
            w,
            &self.uv,
            cw * 2,
        )))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.dims
    }
}
