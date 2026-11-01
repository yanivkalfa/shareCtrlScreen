//! Viewer media thread: decode → present, catching up instead of falling behind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use platform_api::{Codec, Platform, Presenter, VideoSurface};

use crate::session::{serialize, Shared};

/// What the viewer media thread needs.
pub(crate) struct ViewerMedia {
    pub platform: Arc<dyn Platform>,
    pub surface: Arc<dyn VideoSurface>,
    pub codec: Codec,
    pub shared: Arc<Shared>,
}

/// Viewer decode → present loop, with backlog catch-up, a keyframe watchdog and
/// debounced view-size reports. The presenter and decoder are created here.
pub(crate) fn viewer_media_loop(
    media: ViewerMedia,
    video_rx: Receiver<(Vec<u8>, bool)>,
    ctl_tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ViewerMedia {
        platform,
        surface,
        codec,
        shared,
    } = media;

    // Presenter + decoder share one device (zero-copy decode → present). Create
    // both BEFORE revealing the surface, so a failure here never leaves an empty
    // black rectangle covering the UI.
    let mut presenter = surface.create_presenter()?;
    let mut decoder = platform
        .codecs()
        .new_decoder(presenter.device(), codec, 1920, 1080)?;
    tracing::info!("viewer: decoder+presenter ready ({})", codec.as_caps_str());
    surface.show();

    let mut ts = 0i64;
    let mut rendered: u64 = 0;
    let mut render_errors: u64 = 0;
    let mut skipped: u64 = 0;
    // Keyframe watchdog: if AUs arrive but the decoder produces nothing, the
    // keyframe was lost (unreliable channel) — ask the host for a fresh one,
    // rate-limited to ~1/s so a slow link isn't flooded.
    let mut undecoded_streak: u32 = 0;
    let mut last_kf_req = std::time::Instant::now();
    // Catch-up state: after locally dumping a delta backlog, deltas are useless
    // (their reference frames were skipped) until the requested IDR arrives.
    let mut awaiting_keyframe = false;
    // Tell the host what size we're actually displaying at, so it encodes to that
    // and we can present 1:1 (no resampling blur). Debounced: a drag-resize would
    // otherwise rebuild the host's encoder on every intermediate size.
    let mut reported_size: (u32, u32) = (0, 0);
    let mut size_settled_at = std::time::Instant::now();
    let mut pending_size: (u32, u32) = (0, 0);
    // Which of the host's cursor images the presenter holds (D9).
    let mut sprite = InstalledSprite::default();
    while !stop.load(Ordering::SeqCst) {
        // Track window resizes (and hover-reveal offsets) — no-op when unchanged.
        surface.fit();
        {
            let now_size = surface.client_size();
            if now_size != pending_size {
                pending_size = now_size; // still moving — restart the debounce
                size_settled_at = std::time::Instant::now();
            } else if now_size != reported_size
                && now_size.0 >= 320
                && now_size.1 >= 240
                && size_settled_at.elapsed() >= std::time::Duration::from_millis(600)
            {
                reported_size = now_size;
                tracing::info!("viewer: requesting {}x{} from host", now_size.0, now_size.1);
                let _ = ctl_tx.send(serialize(&protocol::ControlMsg::ViewSize {
                    width: now_size.0,
                    height: now_size.1,
                }));
            }
        }
        match video_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(first) => {
                // Batch-drain the queue, then CATCH UP instead of falling behind
                // (the AnyDesk property): software decode at ~20-30ms/frame can
                // never out-decode a backlog, so latency would compound forever.
                let mut pending: Vec<(Vec<u8>, bool)> = vec![first];
                while let Ok(next) = video_rx.try_recv() {
                    pending.push(next);
                }

                if let Some(k) = pending.iter().rposition(|(_, kf)| *kf) {
                    // A keyframe is queued: decoding from the NEWEST one is
                    // always valid — everything older is pure latency. Skip it.
                    if k > 0 {
                        skipped += k as u64;
                        tracing::debug!("viewer: catch-up — skipped {k} stale frame(s)");
                    }
                    pending.drain(..k);
                    awaiting_keyframe = false;
                } else if awaiting_keyframe {
                    // Deltas can't decode until the requested IDR arrives.
                    skipped += pending.len() as u64;
                    if last_kf_req.elapsed() >= std::time::Duration::from_secs(1) {
                        let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                        last_kf_req = std::time::Instant::now();
                    }
                    continue;
                } else if pending.len() > 6 {
                    // Hopeless delta backlog (>~200ms behind, no keyframe in
                    // sight): dump it and resync via a fresh IDR.
                    skipped += pending.len() as u64;
                    awaiting_keyframe = true;
                    tracing::info!(
                        "viewer: {} frame(s) behind — dumped backlog, requesting keyframe",
                        pending.len()
                    );
                    let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                    last_kf_req = std::time::Instant::now();
                    continue;
                }

                // Decode the batch; PRESENT only the newest decoded frame (the
                // earlier ones only exist to carry decoder references forward).
                let count = pending.len();
                for (i, (au, keyframe)) in pending.into_iter().enumerate() {
                    // 100-ns MF units at ~60 fps — decoders (esp. the software
                    // AV1 MFT) can stall on nonsense timestamps like 1,2,3….
                    ts += 166_667;
                    match decoder.decode(&au, keyframe, ts) {
                        Ok(Some(frame)) => {
                            undecoded_streak = 0;
                            rendered += 1;
                            if rendered == 1 || rendered.is_multiple_of(120) {
                                tracing::info!(
                                    "viewer: decoded frame #{rendered} (skipped {skipped} total)"
                                );
                            }
                            if i + 1 < count {
                                continue; // reference-only; present the newest
                            }
                            // Draw the out-of-band cursor sprite on top (§7).
                            let pointer = *shared.cursor.lock();
                            let cursor = pointer.filter(|p| p.visible).map(|p| (p.x, p.y));
                            sprite.sync(presenter.as_mut(), &shared, pointer.and_then(|p| p.shape));
                            // A failing render is NOT silent: this was exactly the
                            // place a black screen hid (decode fine, render dead).
                            if let Err(e) = presenter.present(&frame, cursor) {
                                render_errors += 1;
                                if render_errors <= 10 || render_errors.is_multiple_of(120) {
                                    tracing::warn!("render error #{render_errors}: {e}");
                                }
                            }
                        }
                        Ok(None) => {
                            undecoded_streak += 1;
                            if undecoded_streak >= 5
                                && last_kf_req.elapsed() >= std::time::Duration::from_secs(1)
                            {
                                tracing::info!("viewer: no decodable frames — requesting keyframe");
                                let _ =
                                    ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest));
                                last_kf_req = std::time::Instant::now();
                            }
                        }
                        Err(e) => tracing::warn!("decode error: {e}"),
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// The cursor image installed in the presenter, kept in step with the image
/// the host names (D9). The presenter is told only on a change, because
/// installing an image uploads a texture.
#[derive(Debug, Default)]
struct InstalledSprite {
    /// `None` = the built-in arrow (also the state before any image arrives).
    id: Option<u32>,
}

impl InstalledSprite {
    /// Make the presenter draw image `wanted` (`None` = arrow). An id whose
    /// image has not finished arriving keeps the current picture: the image
    /// is on its way on the reliable channel, and flashing the arrow for
    /// those few milliseconds would look worse than a moment's stale shape.
    fn sync(&mut self, presenter: &mut dyn Presenter, shared: &Shared, wanted: Option<u32>) {
        if wanted == self.id {
            return;
        }
        let sprite = match wanted {
            None => None,
            Some(id) => match shared.cursor_images.lock().get(id) {
                Some(s) => Some(s),
                None => return,
            },
        };
        if let Err(e) = presenter.set_cursor_sprite(sprite.as_deref()) {
            // Recorded as installed anyway: retrying every frame would only
            // repeat the failure; the next shape change tries again.
            tracing::warn!("viewer: cursor image could not be shown: {e}");
        }
        self.id = wanted;
    }
}
