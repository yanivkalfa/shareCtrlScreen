//! The capture -> encode -> decode loop and its per-stage timings.

use std::time::{Duration, Instant};

use platform_api::{
    CaptureError, Codec, CodecFactory, EncoderConfig, Platform, ScreenCapture, VideoDecoder,
    VideoEncoder,
};

/// How long `acquire` waits for a new frame before the loop spins again.
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(32);

/// Frame rate the encoder is told it is being fed. Only rate control reads it.
const ENCODER_FPS: u32 = 30;

/// Per-frame stage timings, in milliseconds, for the frames that went through
/// the whole chain.
#[derive(Debug, Default)]
pub struct Samples {
    pub capture: Vec<f64>,
    pub encode: Vec<f64>,
    pub decode: Vec<f64>,
    /// Capture sessions that had to be rebuilt after `CaptureError::Lost`.
    pub reinits: u32,
}

/// What one pass of the loop produced.
enum Step {
    /// No new pixels (timeout or cursor-only); nothing to time.
    Idle,
    /// The encoder is still warming up and returned no unit.
    Buffered,
    Done {
        cap: f64,
        enc: f64,
        dec: f64,
    },
}

enum StepError {
    /// Recoverable: reinit the capture and force a keyframe.
    Lost,
    Fatal(String),
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Capture, encode and decode on this thread for `duration`.
///
/// `codecs` is passed separately from `platform` so `--software` can swap the
/// factory while the capture side stays the OS one.
pub fn measure(
    platform: &dyn Platform,
    codecs: &dyn CodecFactory,
    display: Option<&str>,
    size: Option<(u32, u32)>,
    duration: Duration,
) -> Result<Samples, String> {
    let mut capture = platform
        .create_capture(display)
        .map_err(|e| format!("capture init failed: {e}"))?;
    if let Some((w, h)) = size {
        if !capture.request_output_size((w, h)) {
            let (nw, nh) = capture.dimensions();
            println!("note: this capture backend cannot scale to {w}x{h}; using {nw}x{nh}");
        }
    }
    // Sized from what the backend really delivers, never from the request.
    let (width, height) = capture.dimensions();
    println!("capture size: {width}x{height}");

    let mut encoder = codecs
        .new_encoder(
            capture.device(),
            EncoderConfig {
                width,
                height,
                fps_num: ENCODER_FPS,
                fps_den: 1,
                codec: Codec::H264,
                ..EncoderConfig::default()
            },
        )
        .map_err(|e| format!("encoder init failed: {e}"))?;
    let mut decoder = codecs
        .new_decoder(capture.device(), Codec::H264, width, height)
        .map_err(|e| format!("decoder init failed: {e}"))?;

    let mut samples = Samples::default();
    let start = Instant::now();
    let deadline = start + duration;
    while Instant::now() < deadline {
        // The captured frame borrows `capture`, so `step` consumes it and hands
        // back only timings; that lets `release` run on every path (a
        // platform-api invariant), errors included.
        let outcome = step(&mut *capture, &mut *encoder, &mut *decoder, start);
        capture.release();
        match outcome {
            Ok(Step::Done { cap, enc, dec }) => {
                samples.capture.push(cap);
                samples.encode.push(enc);
                samples.decode.push(dec);
            }
            Ok(Step::Idle | Step::Buffered) => {}
            Err(StepError::Lost) => {
                capture
                    .reinit()
                    .map_err(|e| format!("capture reinit failed: {e}"))?;
                encoder.force_keyframe();
                samples.reinits += 1;
            }
            Err(StepError::Fatal(msg)) => return Err(msg),
        }
    }
    Ok(samples)
}

fn step(
    capture: &mut dyn ScreenCapture,
    encoder: &mut dyn VideoEncoder,
    decoder: &mut dyn VideoDecoder,
    start: Instant,
) -> Result<Step, StepError> {
    let t0 = Instant::now();
    let captured = match capture.acquire(ACQUIRE_TIMEOUT) {
        Ok(Some(c)) if !c.pointer_only => c,
        Ok(_) => return Ok(Step::Idle),
        Err(CaptureError::Lost) => return Err(StepError::Lost),
        Err(e) => return Err(StepError::Fatal(format!("capture error: {e}"))),
    };
    let cap = ms(t0);

    let t1 = Instant::now();
    // Encode errors are fatal, not skipped frames: a swallowed failure would
    // show up as a flatteringly fast "encode".
    let units = encoder
        .encode(&captured.frame)
        .map_err(|e| StepError::Fatal(format!("encode failed: {e}")))?;
    let enc = ms(t1);
    if units.is_empty() {
        return Ok(Step::Buffered);
    }

    // Decode every unit the encoder produced for this frame, so the decode row
    // covers the whole frame even if a backend emits parameter sets separately.
    let t2 = Instant::now();
    for unit in &units {
        // Media Foundation timestamps are 100 ns ticks.
        let pts_100ns = (start.elapsed().as_nanos() / 100) as i64;
        decoder
            .decode(&unit.data, unit.keyframe, pts_100ns)
            .map_err(|e| StepError::Fatal(format!("decode failed: {e}")))?;
    }
    Ok(Step::Done {
        cap,
        enc,
        dec: ms(t2),
    })
}
