//! Host media thread: capture → encode → hand AUs to the transport, with
//! viewer-size encoding, source pacing against the measured link, and
//! rate-limited on-demand keyframes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use platform_api::{CaptureError, Codec, EncoderConfig, Platform, ScreenCapture};

use crate::session::Shared;
use crate::tuning::{MAX_FPS, MAX_FPS_INTERVAL, MIN_FPS};

/// What the host media thread needs.
pub(crate) struct HostMedia {
    pub platform: Arc<dyn Platform>,
    /// `DisplayInfo::id` to share; `None` = primary.
    pub display: Option<String>,
    pub codec: Codec,
    pub shared: Arc<Shared>,
    /// The reliable control channel, for cursor images (D9): an image is sent
    /// once, so it must not be lost the way a stale position may be.
    pub ctl_tx: Sender<Vec<u8>>,
}

/// Open the capture session, retrying the recoverable case instead of ending the
/// session before it starts. A service-launched host can be briefly off the active
/// input desktop at startup (the W8 regression) — a startup race, or the lock /
/// UAC secure desktop being up — which the backend reports as [`CaptureError::Lost`];
/// we retry with backoff until it returns or the session stops, so the loop
/// self-recovers exactly as it does for a mid-session desktop switch. A missing
/// saved display falls back to the primary; any other (terminal) error is returned.
fn open_capture(
    platform: &dyn Platform,
    display: Option<&str>,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Box<dyn ScreenCapture>, Box<dyn std::error::Error>> {
    let mut backoff = std::time::Duration::from_millis(100);
    let mut waited = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("session stopped before screen capture became available".into());
        }
        let attempt = match platform.create_capture(display) {
            // A saved display that no longer exists (unplugged, or an id from an
            // older version): share the primary one rather than nothing.
            Err(CaptureError::NoOutput) if display.is_some() => {
                // Bind first: `display` as a bare arg to the tracing macro is
                // parsed as its `display(...)` value formatter, not this variable.
                let wanted = display.unwrap_or_default();
                tracing::warn!("display {wanted:?} not found — sharing the primary display");
                platform.create_capture(None)
            }
            other => other,
        };
        match attempt {
            Ok(c) => {
                if waited {
                    tracing::info!(
                        "host: screen capture became available — starting the fast DDA path"
                    );
                }
                return Ok(c);
            }
            Err(CaptureError::Lost) => {
                if !waited {
                    waited = true;
                    tracing::warn!(
                        "host: screen capture not ready yet — the active input desktop is \
                         unavailable (service host starting off the desktop, or a secure \
                         desktop / lock screen is up); retrying until it returns"
                    );
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Host capture → encode loop. Runs on its own thread; the capture session and
/// encoder are created here (they may hold thread-bound GPU/OS objects).
pub(crate) fn host_media_loop(
    media: HostMedia,
    frame_tx: Sender<(Vec<u8>, bool)>,
    cursor_tx: Sender<Vec<u8>>,
    bitrate: Arc<std::sync::atomic::AtomicU32>,
    link: Arc<std::sync::atomic::AtomicU32>,
    force_key: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let HostMedia {
        platform,
        display,
        codec,
        shared,
        ctl_tx,
    } = media;
    // The host's cursor images (D9). Per session: a new session (including a
    // reconnect) starts empty, so the first shape the new capture reports is
    // sent again even if an earlier session had sent it.
    let mut cursor_images = super::cursor::HostCursor::default();
    let codecs = platform.codecs();
    let mut cap = open_capture(platform.as_ref(), display.as_deref(), &stop)?;
    // Encode at the screen's REAL size. Assuming 1920x1080 made the GPU
    // converter rescale every frame on any other monitor (soft picture, wrong
    // aspect) — the viewer adapts to whatever size actually arrives.
    let (mut cap_w, mut cap_h) = cap.dimensions();
    let mut cfg = EncoderConfig {
        codec,
        bitrate_bps: bitrate.load(Ordering::SeqCst),
        // Even dimensions: H.264 chroma is subsampled 2x2 and MFTs reject odd sizes.
        width: (cap_w & !1).max(2),
        height: (cap_h & !1).max(2),
        fps_num: MAX_FPS,
        fps_den: 1,
    };
    // Encoder shares the capture device (§5c zero-copy).
    let mut encoder = codecs.new_encoder(cap.device(), cfg)?;
    tracing::info!(
        "host: capture+encoder ready ({}x{} @{}fps, {}, {} kbps)",
        cfg.width,
        cfg.height,
        MAX_FPS,
        codec.as_caps_str(),
        cfg.bitrate_bps / 1000,
    );
    // The first emitted frame must be an IDR so the viewer can start decoding.
    encoder.force_keyframe();
    let mut applied_bitrate = cfg.bitrate_bps;
    let mut encoder_size = (cfg.width, cfg.height);
    // A viewer size the encoder refused — don't retry it every frame.
    let mut rejected_size: Option<(u32, u32)> = None;
    let mut sent: u64 = 0;
    let mut last_frame_at = std::time::Instant::now();
    // When capture is down (reinit keeps failing because the input desktop is
    // unavailable — secure desktop/lock/switch), remember since when, so we log
    // the loss once and the recovery once rather than on every 250 ms retry.
    let mut capture_down_since: Option<std::time::Instant> = None;

    // Source pacing = the "readable but slower" trade, made explicit. The encoder
    // now has a hard QUALITY floor (its max-QP floor), so when the screen needs more
    // bits than the link can carry it OVERSHOOTS the bitrate instead of blurring.
    // This token bucket absorbs that: we track the bytes actually produced and,
    // when we're ahead of what the link can drain, we skip capturing the next
    // frame rather than let the encoder degrade. Sharpness is preserved; frame
    // rate is what gives. (Bucket is capped at ~1s of credit so an idle screen
    // can still burst a full keyframe out immediately when something changes.)
    let mut debt_bytes: i64 = 0;
    let mut last_debt_at = std::time::Instant::now();

    // Keyframe rate limiting + attribution (see the emit site below). The first
    // keyframe must not be delayed, so start the clock a full gap in the past.
    const MIN_KEYFRAME_GAP: std::time::Duration = std::time::Duration::from_millis(1000);
    let mut last_keyframe_at = std::time::Instant::now() - MIN_KEYFRAME_GAP;
    let mut requested_idr: u32 = 0; // we asked for it (connect / PLI / resize)
    let mut encoder_idr: u32 = 0; // Media Foundation decided on its own
    let mut deferred_keys: u32 = 0; // requests the rate limiter held back
    let mut last_key_log = std::time::Instant::now();

    while !stop.load(Ordering::SeqCst) {
        // Frame-rate cap: never encode faster than MAX_FPS_INTERVAL. The pacing
        // sleep only bites when we're running FASTER than the cap (a busy screen);
        // a slow software encoder or an idle screen sets the real rate.
        let since = last_frame_at.elapsed();
        if since < MAX_FPS_INTERVAL {
            std::thread::sleep(MAX_FPS_INTERVAL - since);
        }
        last_frame_at = std::time::Instant::now();

        // §6 adaptive bitrate: feed the current BWE target to the encoder.
        let target = bitrate.load(Ordering::SeqCst);
        if target != applied_bitrate {
            let _ = encoder.set_bitrate(target);
            applied_bitrate = target;
            cfg.bitrate_bps = target;
        }

        // Encode at the size the viewer actually displays (never upscaling past
        // our own screen), so it presents 1:1 with no resampling. The GPU
        // converter already scales, so this costs nothing extra — and encoding
        // fewer pixels puts more bits into each one. Rebuild only on a material
        // change; the viewer debounces, and every rebuild costs a keyframe.
        let want = match shared.view_size.load(Ordering::SeqCst) {
            0 => (cap_w, cap_h),
            v => {
                let (vw, vh) = ((v >> 32) as u32, v as u32);
                fit_within((vw, vh), (cap_w, cap_h))
            }
        };
        if want != encoder_size && want.0 >= 2 && want.1 >= 2 && rejected_size != Some(want) {
            tracing::info!(
                "host: re-encoding at {}x{} (was {}x{})",
                want.0,
                want.1,
                encoder_size.0,
                encoder_size.1
            );
            cfg.width = want.0;
            cfg.height = want.1;
            match codecs.new_encoder(cap.device(), cfg) {
                Ok(e) => {
                    encoder = e;
                    encoder.force_keyframe(); // new SPS/PPS — restart the decoder
                    applied_bitrate = cfg.bitrate_bps;
                    encoder_size = want;
                    rejected_size = None;
                }
                Err(e) => {
                    // Keep streaming at the working size; remember the bad one so
                    // we don't rebuild-storm on every frame.
                    tracing::warn!("host: encoder rebuild at {want:?} failed ({e}) — keeping size");
                    cfg.width = encoder_size.0;
                    cfg.height = encoder_size.1;
                    rejected_size = Some(want);
                }
            }
        }

        // Where keyframes actually come from — the open question the field logs
        // could not answer, because a request-side counter can't see the ones
        // Media Foundation inserts by itself.
        if last_key_log.elapsed() >= std::time::Duration::from_secs(5) {
            last_key_log = std::time::Instant::now();
            if requested_idr + encoder_idr + deferred_keys > 0 {
                tracing::info!(
                    "keyframes/5s: {requested_idr} requested, {encoder_idr} encoder-initiated, {deferred_keys} deferred"
                );
            }
            requested_idr = 0;
            encoder_idr = 0;
            deferred_keys = 0;
        }

        // Drain the bucket by what the LINK (not the encoder target) could have
        // carried since the last pass. These differ on a bad link: the encoder is
        // floored at MIN_BITRATE to stay legible, so if real capacity is below
        // that, the difference has to come out of the frame rate.
        let link_bps = link.load(Ordering::SeqCst).max(100_000);
        let elapsed = last_debt_at.elapsed();
        last_debt_at = std::time::Instant::now();
        let drained = (link_bps as f64 / 8.0 * elapsed.as_secs_f64()) as i64;
        debt_bytes = (debt_bytes - drained).max(0);
        // More than MIN_FPS's worth of unsent credit? Skip this capture rather
        // than pile more onto a link that hasn't drained the last frame yet.
        let max_debt = (link_bps as i64 / 8) / MIN_FPS as i64;
        if debt_bytes > max_debt && !force_key.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }

        let acquired = cap.acquire(std::time::Duration::from_millis(16));
        match acquired {
            Ok(Some(frame)) => {
                // Cursor moves travel out-of-band on the cursor channel and never
                // wake the video encoder (§5a) — the viewer draws the sprite.
                // Normalize over the CAPTURED frame: the cursor position is in
                // capture pixels, which differ from the encode size whenever the
                // viewer's window is smaller than this screen.
                if let Some(cur) = &frame.cursor {
                    // A changed image goes out on the reliable channel BEFORE the
                    // position naming it, so the viewer usually has the image by
                    // the time it is asked to draw it (until then it keeps the
                    // image it was drawing).
                    let frame_size = (frame.frame.width, frame.frame.height);
                    for part in cursor_images.update(cur.shape.as_ref(), frame_size) {
                        let _ = ctl_tx.send(serde_json::to_vec(&part).unwrap_or_default());
                    }
                    let x =
                        (cur.position.x as f64 / frame.frame.width.max(1) as f64).clamp(0.0, 1.0);
                    let y =
                        (cur.position.y as f64 / frame.frame.height.max(1) as f64).clamp(0.0, 1.0);
                    let msg = protocol::ControlMsg::Cursor {
                        x,
                        y,
                        shape: cursor_images.current(),
                        visible: cur.visible,
                    };
                    let _ = cursor_tx.send(serde_json::to_vec(&msg).unwrap_or_default());
                }

                // Any non-pointer-only frame carries new pixels (an empty dirty
                // list means the backend has no damage information).
                let has_change = !frame.pointer_only;

                // KEYFRAMES ON DEMAND ONLY (infinite GOP — the AnyDesk model).
                // No periodic IDR: at CBR every IDR costs ~10x a delta, so the
                // encoder crushes its quality and deltas spend the next second
                // sharpening — a visible blur PULSE every interval ("randomly
                // blurry, then not"). No scene-change IDR either: a full-screen
                // change is just a big delta the encoder intra-codes anyway.
                // Corruption can't linger because the viewer NEVER decodes across
                // a loss gap (it drops deltas and requests an IDR — see the
                // transport driver), so on-demand recovery covers everything:
                // connect, RTCP PLI, ctl KeyframeRequest, capture AccessLost.
                // RATE-LIMITED. Field logs showed 3-5 IDRs every 5 seconds on
                // idle content, and each one is a burst big enough to spike the
                // relay's queuing delay — which congestion control reads as
                // congestion, collapsing the estimate from ~5 Mbps to ~150 kbps.
                // The pacer then starves the viewer, the starved viewer asks for
                // a keyframe, and the loop feeds itself. Honouring at most one
                // request per MIN_KEYFRAME_GAP breaks it at the narrowest point.
                // A request is never dropped, only deferred: `force_key` stays
                // set and fires as soon as the gap has elapsed.
                let want_key = force_key.load(Ordering::SeqCst);
                let key_now = want_key && last_keyframe_at.elapsed() >= MIN_KEYFRAME_GAP;
                if want_key && !key_now {
                    deferred_keys += 1;
                }
                if !has_change && !key_now {
                    drop(frame);
                    cap.release();
                    continue;
                }
                if key_now {
                    encoder.force_keyframe();
                    force_key.store(false, Ordering::SeqCst);
                    last_keyframe_at = std::time::Instant::now();
                }
                // BGRA→NV12 + encode happen inside the encoder path (§5b).
                match encoder.encode(&frame.frame) {
                    Ok(units) => {
                        for u in units {
                            sent += 1;
                            // Attribute every IDR. An IDR we did NOT ask for is
                            // the encoder's own doing (Media Foundation runs its
                            // own scene-change detection and honours GOPSize), and
                            // that is the one keyframe source we cannot see from
                            // the request side — so count it separately.
                            if u.keyframe {
                                if key_now {
                                    requested_idr += 1;
                                } else {
                                    encoder_idr += 1;
                                }
                            }
                            // Charge the pacing bucket with what we actually
                            // produced — with a QP floor this can exceed the CBR
                            // target, and paying for it in frames is the point.
                            debt_bytes += u.data.len() as i64;
                            if sent == 1 || sent.is_multiple_of(120) {
                                tracing::info!(
                                    "host: encoded+sent AU #{sent} ({} bytes, keyframe={}, bitrate={}kbps)",
                                    u.data.len(),
                                    u.keyframe,
                                    applied_bitrate / 1000
                                );
                            }
                            let _ = frame_tx.send((u.data, u.keyframe));
                        }
                    }
                    Err(e) => tracing::warn!("encode error: {e}"),
                }
                drop(frame);
                cap.release();
            }
            Ok(None) => { /* §5a idle: WAIT_TIMEOUT, send nothing */ }
            // The one recoverable error (mode change, display sleep, desktop
            // switch, or a service host off the input desktop — the W8 regression,
            // which the Windows backend maps here instead of ending the session).
            // `reinit` re-attaches the capture thread to the active input desktop
            // and re-acquires the duplication; if that still fails (secure desktop
            // up), back off and keep trying until the desktop returns.
            Err(CaptureError::Lost) => {
                if let Err(e) = cap.reinit() {
                    if capture_down_since.is_none() {
                        capture_down_since = Some(std::time::Instant::now());
                        tracing::warn!(
                            "host: screen capture lost (reinit failed: {e}) — the active input \
                             desktop is unavailable (secure desktop / lock screen / session \
                             switch); retrying until it returns. This is a CAPTURE restart, not \
                             an encoder change."
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    continue;
                }
                if let Some(since) = capture_down_since.take() {
                    tracing::info!(
                        "host: screen capture re-acquired after {:?} — fast DDA path restored \
                         (capture restart, not an encoder change)",
                        since.elapsed()
                    );
                }
                // A mode change alters the screen size: re-read it, and the size
                // check above rebuilds the encoder on the next pass.
                let (w, h) = cap.dimensions();
                if (w, h) != (cap_w, cap_h) {
                    tracing::info!("host: capture size changed to {w}x{h}");
                    cap_w = w;
                    cap_h = h;
                }
                encoder.force_keyframe(); // new surface — resync the viewer
            }
            Err(e) => {
                tracing::warn!("capture: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// The largest `src`-aspect size that fits inside `want`, never upscaling past
/// `src`, rounded to even dimensions (H.264 chroma is subsampled 2x2 and MFTs
/// reject odd sizes). Preserving the source aspect matters: the viewer letterboxes
/// to the same ratio, so anything else would be displayed with bars AND rescaled.
pub(crate) fn fit_within(want: (u32, u32), src: (u32, u32)) -> (u32, u32) {
    if want.0 == 0 || want.1 == 0 || src.0 == 0 || src.1 == 0 {
        return src;
    }
    // Scale by the tighter of the two axes, capped at 1.0 (no upscaling — it
    // would cost bitrate without adding any real detail).
    let sx = want.0 as f64 / src.0 as f64;
    let sy = want.1 as f64 / src.1 as f64;
    let s = sx.min(sy).min(1.0);
    let w = ((src.0 as f64 * s).round() as u32) & !1;
    let h = ((src.1 as f64 * s).round() as u32) & !1;
    (w.max(2), h.max(2))
}

#[cfg(test)]
mod tests {
    use super::fit_within;

    #[test]
    fn fit_within_never_upscales() {
        // A viewer window bigger than the screen must not inflate the encode.
        assert_eq!(fit_within((3840, 2160), (1920, 1080)), (1920, 1080));
    }

    #[test]
    fn fit_within_preserves_aspect_and_evenness() {
        // Maximized 1080p viewer: some height lost to chrome. The result keeps
        // 16:9 (the viewer letterboxes to that) and stays even for 4:2:0 chroma.
        let (w, h) = fit_within((1920, 1040), (1920, 1080));
        assert_eq!((w, h), (1848, 1040));
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn fit_within_handles_degenerate_input() {
        // Unknown/zero sizes fall back to the source rather than producing 0x0.
        assert_eq!(fit_within((0, 0), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_within((1280, 720), (0, 0)), (0, 0));
    }
}
