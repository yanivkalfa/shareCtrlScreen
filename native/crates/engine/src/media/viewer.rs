//! Viewer media thread: decode → present, catching up instead of falling behind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use platform_api::{Codec, Frame, Platform, Presenter, VideoSurface};

use crate::session::{serialize, Shared};

/// What the viewer media thread needs.
pub(crate) struct ViewerMedia {
    pub platform: Arc<dyn Platform>,
    pub surface: Arc<dyn VideoSurface>,
    pub codec: Codec,
    pub shared: Arc<Shared>,
    /// The video stream shown (0 = main window; 1.. = a pop-out, D14). Tags
    /// this loop's keyframe requests and size reports.
    pub slot: u8,
    /// The main video's gate (P3): it decides when the surface is shown,
    /// and the loop presents only while it is. `None` for a pop-out, which
    /// shows itself.
    pub view: Option<Arc<crate::session::active::SessionView>>,
    /// The main video's moves to other windows (plans/09 W2); `None` for a
    /// pop-out, which stays in its own window.
    pub moves: Option<Moves>,
}

/// What carrying out a move takes.
pub(crate) struct Moves {
    pub cell: Arc<crate::session::current::SurfaceCell>,
    pub ui: crate::SessionUi,
}

/// Idle repaint cadence (D11): how long the loop blocks for a new frame before
/// repainting the cursor on the last one. ~60 Hz so a static host screen still
/// tracks the local pointer smoothly; a flowing stream returns sooner on data.
const IDLE_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// How long after the host's screen came back the viewer waits for the
/// keyframe that restarts the picture before it asks for one itself. Longer
/// than a keyframe normally takes to arrive behind the message, so the usual
/// case sends no request; short enough that a still screen is not left black.
const POST_BLOCK_KEYFRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

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
        slot,
        view,
        moves,
    } = media;
    let mut surface = surface;

    // Presenter + decoder share one device (zero-copy decode → present). Create
    // both BEFORE revealing the surface, so a failure here never leaves an empty
    // black rectangle covering the UI.
    let mut presenter = surface.create_presenter()?;
    let mut decoder = platform
        .codecs()
        .new_decoder(presenter.device(), codec, 1920, 1080)?;
    tracing::info!("viewer: decoder+presenter ready ({})", codec.as_caps_str());
    // A pop-out shows itself, but not while the host's screen is blocked
    // (K21): it then owes its show until the block ends. The transport
    // thread shows the pop-outs it knows of when the block ends; this
    // covers one that was not registered yet at that moment.
    let mut owes_show = false;
    match &view {
        Some(v) => v.set_ready(),
        None => owes_show = !show_unless_blocked(surface.as_ref(), &shared),
    }

    let mut ts = 0i64;
    let mut rendered: u64 = 0;
    let mut render_errors: u64 = 0;
    let mut skipped: u64 = 0;
    // Decode errors since the last picture: the first few log the access
    // unit's first bytes, so a stream no decoder takes can be told apart
    // from loss in the log (the 2.3.0 macOS AV1 report).
    let mut decode_errors: u32 = 0;
    // Keyframe watchdog: if AUs arrive but the decoder produces nothing, the
    // keyframe was lost (unreliable channel) — ask the host for a fresh one,
    // rate-limited to ~1/s so a slow link isn't flooded.
    let mut undecoded_streak: u32 = 0;
    let mut last_kf_req = std::time::Instant::now();
    // Catch-up state: after locally dumping a delta backlog, deltas are useless
    // (their reference frames were skipped) until the requested IDR arrives.
    let mut awaiting_keyframe = false;
    // When the host's last block ended, while the keyframe that restarts the
    // picture after it has not come yet (plans/10 U1, K21). See the top of the
    // loop and the idle tick.
    let mut block_ended_at: Option<std::time::Instant> = None;
    // Whether this loop has asked for that keyframe since the block ended
    // (for the log level only).
    let mut post_block_asked = false;
    // Tell the host what size we're actually displaying at, so it encodes to that
    // and we can present 1:1 (no resampling blur). Debounced: a drag-resize would
    // otherwise rebuild the host's encoder on every intermediate size.
    let mut reported_size: (u32, u32) = (0, 0);
    let mut size_settled_at = std::time::Instant::now();
    let mut pending_size: (u32, u32) = (0, 0);
    // Which of the host's cursor images the presenter holds (D9).
    let mut sprite = InstalledSprite::default();
    // D11: the newest decoded frame, held so the cursor can be repainted
    // BETWEEN video frames — a static host screen sends few/no frames, yet the
    // local pointer keeps moving, so the drawn sprite would otherwise freeze and
    // only snap forward on the next frame. It borrows the decoder's current
    // surface, so it MUST be dropped (`= None`) before the next `decode` call.
    // `last_cursor` is where that frame was last presented, so the idle tick
    // re-presents only when the cursor actually moved (bounding the GPU work).
    let mut last_frame: Option<Frame<'_>> = None;
    let mut last_cursor: Option<(f64, f64)> = None;
    // The host-block epoch `last_frame` was decoded in (plans/10 U1, K21).
    // A held frame from before a block is never presented again: not by the
    // repaint that follows the re-show, not by a cursor move. Compared at the
    // top of each pass (so even a block that ended before this loop looked
    // drops it) and again right before any present: a block can arrive while
    // a pass waits for or decodes a frame.
    let mut held_epoch = shared.host_block.epoch();
    while !stop.load(Ordering::SeqCst) {
        let epoch = shared.host_block.epoch();
        let host_blocked = epoch % 2 == 1;
        if epoch != held_epoch {
            last_frame = None;
            held_epoch = epoch;
            // The control channel and video are not ordered against each
            // other. Frames encoded before the block can still be in flight
            // (the host's pacer, the network, this queue) when the block's end
            // has already arrived, and decoding them would show the screen
            // from before the lock after it (K21; seen on a slow machine,
            // where they came a second late). The host starts again with a
            // keyframe (10a K5), so nothing older than the next keyframe is
            // decoded: the catch-up state below skips deltas and asks for a
            // keyframe, at most once a second, until one comes.
            awaiting_keyframe = true;
            // The other order: that keyframe can arrive before the block's
            // end does, and is then decoded while still blocked, so never
            // shown. On a still screen nothing would follow it. The idle tick
            // asks for another once the block has been over for a moment.
            block_ended_at = (!host_blocked).then(std::time::Instant::now);
            post_block_asked = false;
        }
        if owes_show && !host_blocked {
            owes_show = !show_unless_blocked(surface.as_ref(), &shared);
        }
        // A move to another window (plans/09 W2): a presenter on the new
        // surface, on the decoder's device — the decoder and its held frame
        // stay, so nothing waits for a keyframe an idle host would not send.
        // The view shows the new surface and asks for a repaint, which the
        // idle tick below answers with the held frame.
        if let Some(m) = &moves {
            if let Some(target) = m.cell.pending() {
                match target.surface.create_presenter_on(presenter.device()) {
                    Ok(next) => match m.cell.complete() {
                        Some(old) => {
                            // The old presenter goes before the app may
                            // destroy the old surface's window.
                            drop(std::mem::replace(&mut presenter, next));
                            surface = target.surface;
                            sprite = InstalledSprite::default();
                            last_cursor = None;
                            tracing::info!("viewer: moved to window {}", target.window.0);
                            m.ui.send(crate::SessionEvent::SurfaceReleased {
                                surface: crate::SurfaceTag::of(&old),
                            });
                        }
                        // Ended meanwhile: `Released` covers both surfaces.
                        None => drop(next),
                    },
                    Err(e) => {
                        tracing::warn!("viewer: could not move to the new window: {e}");
                        if let Some(t) = m.cell.fail() {
                            m.ui.send(crate::SessionEvent::SurfaceReleased {
                                surface: crate::SurfaceTag::of(&t),
                            });
                        }
                        m.ui.send(crate::SessionEvent::Toast(format!(
                            "Could not move the remote screen to that window: {e}"
                        )));
                    }
                }
            }
        }
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
                    s: slot,
                }));
            }
        }
        // A short idle timeout (~16 ms) so a static screen still repaints the
        // cursor at ~60 Hz (D11); when frames are flowing, `recv_timeout`
        // returns at once on the queued data, so this adds no video latency.
        match video_rx.recv_timeout(IDLE_REPAINT_INTERVAL) {
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
                    block_ended_at = None;
                } else if awaiting_keyframe {
                    // Deltas can't decode until the requested IDR arrives.
                    skipped += pending.len() as u64;
                    if last_kf_req.elapsed() >= std::time::Duration::from_secs(1) {
                        let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest {
                            s: slot,
                        }));
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
                    let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest {
                        s: slot,
                    }));
                    last_kf_req = std::time::Instant::now();
                    continue;
                }

                // Decode the batch; PRESENT only the newest decoded frame (the
                // earlier ones only exist to carry decoder references forward).
                // Drop the held frame FIRST: it borrows the decoder's surface,
                // which the next `decode` overwrites (D11).
                last_frame = None;
                let count = pending.len();
                let mut pending = pending.into_iter();
                // All but the newest are reference-only: decode to carry the
                // decoder's references forward, present nothing.
                for _ in 1..count {
                    let (au, keyframe) = pending.next().expect("count-1 reference frames");
                    // 100-ns MF units at ~60 fps — decoders (esp. the software
                    // AV1 MFT) can stall on nonsense timestamps like 1,2,3….
                    ts += 166_667;
                    match decoder.decode(&au, keyframe, ts) {
                        Ok(Some(_)) => {
                            note_decoded(&mut rendered, &mut undecoded_streak, skipped);
                            decode_errors = 0;
                        }
                        Ok(None) => {
                            on_no_picture(&mut undecoded_streak, &mut last_kf_req, &ctl_tx, slot)
                        }
                        Err(e) => note_decode_error(&mut decode_errors, &e, &au),
                    }
                }
                // The newest frame: decode, draw the cursor on top, present, and
                // KEEP it so the idle tick can repaint the cursor on it.
                if let Some((au, keyframe)) = pending.next() {
                    ts += 166_667;
                    match decoder.decode(&au, keyframe, ts) {
                        Ok(Some(frame)) => {
                            note_decoded(&mut rendered, &mut undecoded_streak, skipped);
                            decode_errors = 0;
                            // The SHAPE is always the host's (D9); the POSITION
                            // follows the LOCAL pointer while controlling over the
                            // video (D10/D11 — instant, no round-trip lag), and
                            // otherwise the host's out-of-band position.
                            let (cursor, shape) = current_cursor(&shared, surface.as_ref());
                            // A hidden video (another tab is active, a dialog
                            // covers it) is decoded, so its reference frames
                            // stay current, but not presented (B5). Even a
                            // background tab decodes what still arrives (its
                            // host pauses within a round trip, P6): the held
                            // frame is then exactly the screen at the pause,
                            // which is what an idle host — sending nothing
                            // until its screen changes — shows on return.
                            // Read again, not this pass's copy: a block that
                            // came while decoding stops the present (K21).
                            let block_changed = shared.host_block.epoch() != held_epoch;
                            if !host_blocked
                                && !block_changed
                                && view.as_ref().is_none_or(|v| v.is_shown())
                            {
                                present_frame(
                                    presenter.as_mut(),
                                    &shared,
                                    &mut sprite,
                                    &frame,
                                    cursor,
                                    shape,
                                    &mut render_errors,
                                );
                            }
                            last_cursor = cursor;
                            // Decoded while blocked (in flight from before) or
                            // across a change: kept for its references only,
                            // never held.
                            if !host_blocked && !block_changed {
                                last_frame = Some(frame);
                            }
                        }
                        Ok(None) => {
                            on_no_picture(&mut undecoded_streak, &mut last_kf_req, &ctl_tx, slot)
                        }
                        Err(e) => note_decode_error(&mut decode_errors, &e, &au),
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // No new frame arrived. Repaint the LAST decoded frame when
                // either the cursor moved (D11 — keep the local pointer tracking
                // on a static screen) or a one-shot repaint was requested (D12 —
                // the surface was just re-shown, e.g. Settings closed, and would
                // otherwise stay black until the next frame). `swap` reads and
                // clears the request atomically so it fires exactly once.
                let repaint = shared.repaint_requested.swap(false, Ordering::SeqCst);
                // The block is over and no keyframe has come since (see the
                // top of the loop): ask for one, so a still screen does not
                // stay black. The host defers a request that follows its own
                // keyframe too closely, and never drops one. Only while the
                // video is on screen: a background tab's stream is paused and
                // its host sends nothing, so asking would repeat for as long
                // as the tab stays there; when it returns, this tick asks.
                if block_ended_at.is_some_and(|at| at.elapsed() >= POST_BLOCK_KEYFRAME_WAIT)
                    && last_kf_req.elapsed() >= std::time::Duration::from_secs(1)
                    && view.as_ref().is_none_or(|v| v.is_shown())
                {
                    // Once at info; a host that cannot answer yet (a still
                    // screen delivers it no frame to encode) is asked again
                    // every second, and that is not worth a line each time.
                    if post_block_asked {
                        tracing::debug!(
                            "viewer: still no keyframe since the host's screen came back"
                        );
                    } else {
                        tracing::info!("viewer: no keyframe since the host's screen came back — requesting one");
                    }
                    post_block_asked = true;
                    let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest {
                        s: slot,
                    }));
                    last_kf_req = std::time::Instant::now();
                }
                // Looked at again: a block (or a block and its end) may have
                // come while this pass waited, and the re-show's repaint
                // must not find the frame from before it (K21).
                if shared.host_block.epoch() != held_epoch {
                    last_frame = None;
                }
                match &last_frame {
                    Some(frame) => {
                        let (cursor, shape) = current_cursor(&shared, surface.as_ref());
                        let shown = view.as_ref().is_none_or(|v| v.is_shown());
                        if shown && should_idle_repaint(repaint, cursor, last_cursor) {
                            present_frame(
                                presenter.as_mut(),
                                &shared,
                                &mut sprite,
                                frame,
                                cursor,
                                shape,
                                &mut render_errors,
                            );
                            last_cursor = cursor;
                        }
                    }
                    // D12: asked to repaint but nothing has decoded yet — ask the
                    // host for a fresh keyframe (rate-limited) so a frame arrives
                    // to show, instead of leaving the surface black.
                    None => {
                        if repaint && last_kf_req.elapsed() >= std::time::Duration::from_secs(1) {
                            let _ =
                                ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest {
                                    s: slot,
                                }));
                            last_kf_req = std::time::Instant::now();
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Show a pop-out's surface unless the host's screen is blocked; returns
/// whether it is shown. Re-checked after the show: a block that starts
/// between the check and the show finds the surface not yet registered or not
/// yet shown, so the transport thread's hide may have come first — this
/// hides it again and the show stays owed (K21).
fn show_unless_blocked(surface: &dyn VideoSurface, shared: &Shared) -> bool {
    if shared.host_block.blocked() {
        return false;
    }
    surface.show();
    if shared.host_block.blocked() {
        surface.hide();
        return false;
    }
    true
}

/// A decode error: logged, with the access unit's first bytes for the
/// first few in a row (then only the error, so a broken stream does not
/// flood the log).
fn note_decode_error(count: &mut u32, e: &dyn std::fmt::Display, au: &[u8]) {
    *count = count.saturating_add(1);
    if *count <= 3 {
        tracing::warn!(
            "decode error: {e} ({} bytes: {})",
            au.len(),
            crate::media::av1::hex_prefix(au)
        );
    } else {
        tracing::warn!("decode error: {e}");
    }
}

/// Bookkeeping for a decoded picture: reset the keyframe watchdog and log a
/// heartbeat. Shared by the reference-only and newest-frame decode paths (D11).
fn note_decoded(rendered: &mut u64, undecoded_streak: &mut u32, skipped: u64) {
    *undecoded_streak = 0;
    *rendered += 1;
    if *rendered == 1 || rendered.is_multiple_of(120) {
        tracing::info!("viewer: decoded frame #{rendered} (skipped {skipped} total)");
    }
}

/// The decoder produced no picture: bump the watchdog and, after a short streak,
/// ask the host for a fresh keyframe (rate-limited to ~1/s so a slow link is
/// not flooded). Shared by both decode paths (D11).
fn on_no_picture(
    undecoded_streak: &mut u32,
    last_kf_req: &mut std::time::Instant,
    ctl_tx: &Sender<Vec<u8>>,
    slot: u8,
) {
    *undecoded_streak += 1;
    if *undecoded_streak >= 5 && last_kf_req.elapsed() >= std::time::Duration::from_secs(1) {
        tracing::info!("viewer: no decodable frames — requesting keyframe");
        let _ = ctl_tx.send(serialize(&protocol::ControlMsg::KeyframeRequest {
            s: slot,
        }));
        *last_kf_req = std::time::Instant::now();
    }
}

/// The cursor to draw right now and the host's shape id (task D10/D11), from the
/// shared state and the local pointer. Split out so the decoded-frame present
/// and the idle repaint compute it identically.
fn current_cursor(
    shared: &Shared,
    surface: &dyn VideoSurface,
) -> (Option<(f64, f64)>, Option<u32>) {
    let pointer = *shared.cursor.lock();
    let host_cursor = pointer.filter(|p| p.visible).map(|p| (p.x, p.y));
    let cursor = cursor_position(
        shared.control.load(Ordering::SeqCst),
        surface.local_pointer(),
        host_cursor,
    );
    (cursor, pointer.and_then(|p| p.shape))
}

/// Sync the cursor sprite shape, then present `frame` with `cursor`. A failing
/// render is NOT silent — this is exactly where a black screen once hid (decode
/// fine, render dead). Shared by the decoded-frame and idle repaint paths (D11).
fn present_frame(
    presenter: &mut dyn Presenter,
    shared: &Shared,
    sprite: &mut InstalledSprite,
    frame: &Frame<'_>,
    cursor: Option<(f64, f64)>,
    shape: Option<u32>,
    render_errors: &mut u64,
) {
    sprite.sync(presenter, shared, shape);
    if let Err(e) = presenter.present(frame, cursor) {
        *render_errors += 1;
        if *render_errors <= 10 || render_errors.is_multiple_of(120) {
            tracing::warn!("render error #{render_errors}: {e}");
        }
    }
}

/// Whether the idle tick should re-present the held frame. Re-present on an
/// explicit repaint request (D12 — the surface was just re-shown, so paint even
/// with the cursor stationary) OR when the cursor actually moved (D11 — keep the
/// local pointer tracking on a static screen while bounding the GPU work).
/// Split out so the signal is unit-testable without a real decoder/presenter.
fn should_idle_repaint(
    repaint_requested: bool,
    cursor: Option<(f64, f64)>,
    last_cursor: Option<(f64, f64)>,
) -> bool {
    repaint_requested || cursor != last_cursor
}

/// Choose the cursor sprite POSITION (task D10/D11). The SHAPE is always the
/// host's (D9); only the position source changes here.
///
/// * controlling AND the local pointer is over the video (`local` is `Some`) →
///   the LOCAL position, ALWAYS: instant, under the real mouse (same mapping as
///   the input), and — the D11 fix — it must NOT vanish when the host's
///   out-of-band cursor report momentarily blips to absent (a dropped
///   unreliable-channel update, or the host hiding its own cursor);
/// * otherwise the host's out-of-band position — view-only, or controlling with
///   the pointer not over the surface. `host` is already filtered by visibility,
///   so on this path an invisible host cursor (`host == None`) draws nothing.
fn cursor_position(
    control: bool,
    local: Option<(f64, f64)>,
    host: Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    match control.then_some(local).flatten() {
        Some(local_pos) => Some(local_pos),
        None => host,
    }
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

#[cfg(test)]
mod tests {
    use super::{cursor_position, should_idle_repaint};

    const LOCAL: (f64, f64) = (0.2, 0.3);
    const HOST: (f64, f64) = (0.8, 0.9);

    #[test]
    fn repaint_request_repaints_even_with_a_stationary_cursor() {
        // D12: Settings closed → the surface was re-shown and a repaint was
        // requested. The idle tick must re-present the held frame ONCE even
        // though the cursor has not moved, so the video returns immediately
        // instead of staying black on a static host screen.
        let cursor = Some(LOCAL);
        assert!(
            should_idle_repaint(true, cursor, cursor),
            "a repaint request must repaint regardless of cursor movement"
        );
    }

    #[test]
    fn no_request_and_a_stationary_cursor_does_not_repaint() {
        // D11 unchanged: without a request, a static screen with a still cursor
        // must NOT repaint every idle tick (bounds the GPU work).
        let cursor = Some(LOCAL);
        assert!(!should_idle_repaint(false, cursor, cursor));
    }

    #[test]
    fn a_moved_cursor_repaints_without_a_request() {
        // D11 unchanged: the local pointer moved on a static screen → repaint so
        // the drawn sprite tracks it.
        assert!(should_idle_repaint(false, Some(LOCAL), Some(HOST)));
    }

    #[test]
    fn controlling_and_over_the_video_uses_the_local_pointer() {
        assert_eq!(
            cursor_position(true, Some(LOCAL), Some(HOST)),
            Some(LOCAL),
            "instant: the real mouse, not the host's delayed echo"
        );
    }

    #[test]
    fn view_only_always_follows_the_host() {
        assert_eq!(
            cursor_position(false, Some(LOCAL), Some(HOST)),
            Some(HOST),
            "a view-only viewer shows the host's own cursor moving"
        );
    }

    #[test]
    fn pointer_not_over_the_surface_follows_the_host() {
        // Controlling, but the local pointer is elsewhere (`None`): a host-
        // initiated move must still be reflected.
        assert_eq!(cursor_position(true, None, Some(HOST)), Some(HOST));
    }

    #[test]
    fn controlling_over_the_video_survives_a_host_cursor_blip() {
        // D11 fix: the host's out-of-band cursor report is momentarily absent
        // (a dropped unreliable-channel update, or the host hid its own cursor)
        // while we control with the pointer inside. The sprite must STAY at the
        // local pointer, not vanish and snap back when the next report arrives.
        assert_eq!(
            cursor_position(true, Some(LOCAL), None),
            Some(LOCAL),
            "controlling + pointer inside always draws at the local pointer"
        );
    }

    #[test]
    fn an_invisible_host_cursor_is_never_drawn_in_view_only() {
        // View-only (or controlling with the pointer not over the surface):
        // `host == None` (the host hid its cursor, D9 visible==false) draws
        // nothing — there is no local pointer to fall back on.
        assert_eq!(cursor_position(false, Some(LOCAL), None), None);
        assert_eq!(cursor_position(false, None, None), None);
        assert_eq!(cursor_position(true, None, None), None);
    }
}
