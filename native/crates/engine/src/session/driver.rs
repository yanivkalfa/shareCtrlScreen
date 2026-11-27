//! The transport driver: one thread per session that owns the str0m `Rtc`, the
//! UDP socket and the optional TURN allocation, and runs the sans-IO loop —
//! liveness, keyframe gating, bandwidth estimation, control/input/clipboard/file
//! routing. OS access goes through `platform_api` only.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use platform_api::{InputInjector, Platform, Rect, VideoSurface};
use protocol::SignalData;

use super::{serialize, Shared, DESIRED_BITRATE};

/// Apply one bandwidth estimate to the host's rate state. Shared by the real
/// str0m estimate and (under the `test-hooks` feature) an injected one, so both
/// take exactly the same path.
///
/// Two outputs come from the one estimate:
/// * `link` — the RAW capacity, unfloored: the telemetry / test-observation value
///   (the loopback asserts on it). V2 has no resolution ladder, so nothing steers
///   the encode size from it; it records what the estimate actually read.
/// * `bitrate` — the encoder's CBR target. V3 no longer applies the raw estimate:
///   it folds the estimate into a [`RateFilter`] (a light EMA + asymmetric
///   sustained-window gate) and applies the SMOOTHED target. The encoder is
///   reconfigured only when the smoothed target actually changes — which, on a
///   noisy-but-stable link, is rarely — so the full-resolution picture stays
///   STEADY instead of pulsing with every GCC swing. This is still the ONLY thing
///   the link estimate steers; the encode size never changes with it.
///
/// `dt_secs` is the wall-clock time since the previous estimate; the filter's EMA
/// is time-based, so this path behaves the same whether called once per real
/// estimate or re-applied every tick under the test hook.
fn apply_bwe(
    bps: u32,
    dt_secs: f64,
    filter: &mut crate::tuning::RateFilter,
    bitrate: &Option<Arc<std::sync::atomic::AtomicU32>>,
    link: &Option<Arc<std::sync::atomic::AtomicU32>>,
) {
    if let Some(lk) = link {
        lk.store(bps, Ordering::SeqCst);
    }
    if let Some(br) = bitrate {
        let prev = filter.target();
        *filter = filter.next(bps, dt_secs);
        let next = filter.target();
        if next != prev {
            tracing::info!(
                "bwe: link {} kbps → encoder {} kbps (smoothed)",
                bps / 1000,
                next / 1000
            );
            br.store(next, Ordering::SeqCst);
        }
    }
}

/// Whether a test is currently forcing the bandwidth estimate (so the real str0m
/// estimate must be ignored). Always `false` without the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
fn bwe_overridden(shared: &Shared) -> bool {
    shared.bwe_inject.load(Ordering::Relaxed) != 0
}
#[cfg(not(feature = "test-hooks"))]
fn bwe_overridden(_shared: &Shared) -> bool {
    false
}

/// Bundle of the role-specific channels/flags handed to [`transport_driver`].
pub(super) struct Driver {
    pub(super) rtc: str0m::Rtc,
    pub(super) pending: Option<str0m::change::SdpPendingOffer>,
    pub(super) signal_rx: Receiver<SignalData>,
    pub(super) ctl_rx: Receiver<Vec<u8>>,
    /// Host: encoded AUs to send, tagged with their video stream slot.
    pub(super) frame_rx: Option<Receiver<(u8, Vec<u8>, bool)>>,
    /// Viewer: reassembled AUs (+ keyframe flag) out to the decode/render thread.
    pub(super) video_tx: Option<Sender<(Vec<u8>, bool)>>,
    /// Host: injection gate (Some ⇒ this side injects remote input).
    pub(super) inject: Option<Arc<AtomicBool>>,
    /// Host: serialized cursor updates to send on the cursor channel.
    pub(super) cursor_rx: Option<Receiver<Vec<u8>>>,
    /// Viewer: encoded [`protocol::BulkFrame`]s for an outgoing file transfer.
    /// Drained with backpressure so a large file can't monopolise the link.
    pub(super) bulk_rx: Option<Receiver<Vec<u8>>>,
    /// Clipboard text received from the peer, handed to the clipboard thread
    /// (which owns the OS clipboard and the echo guard).
    pub(super) clip_tx: Sender<String>,
    /// Host: the §6 data-channel ids created on the Rtc in `begin_host`.
    pub(super) channels: Option<transport::Channels>,
    /// The video RTP media tracks' mids in stream-slot order (0 = main, then
    /// the pop-out spares, D14). Host: from `add_media`. Viewer: read from the
    /// offer (empty if it could not be: `Event::MediaAdded` fills it in).
    pub(super) video_mids: Vec<str0m::media::Mid>,
    /// Host: set to make the encoder emit an IDR (on video-channel open, and on a
    /// viewer `KeyframeRequest`). Frames sent before the channel opened are lost
    /// on the wire, so the first *deliverable* frame must restart the decoder.
    pub(super) force_key: Option<Arc<AtomicBool>>,
    /// Host: the encoder's CBR target — the BWE estimate clamped to
    /// `[MIN_BITRATE, TARGET_BITRATE]`. It FOLLOWS the estimate down to a low floor
    /// and back up; this is the ONLY thing a constrained link changes — the encode
    /// resolution stays full, so the picture softens but stays steady (V2 removed
    /// V1's resolution ladder — see `crate::tuning`).
    pub(super) bitrate: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Host: the RAW measured link capacity from BWE, unfloored. Recorded for
    /// telemetry and loopback observation; V2 does not steer the encode size from
    /// it (no resolution ladder), so it only tracks what the estimate read.
    pub(super) link: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Where its events go, tagged with this session (the driver thread has
    /// no `&Engine`).
    pub(super) ui: crate::SessionUi,
    /// Set once the session ends, by whichever side notices first: this
    /// thread on a dead link, or the engine on a disconnect.
    pub(super) ended: Arc<AtomicBool>,
    /// Where a dead link is reported, for the engine to end this session.
    pub(super) end_tx: tokio::sync::mpsc::UnboundedSender<crate::EndRequest>,
    pub(super) id: crate::SessionId,
    /// The bound UDP socket (candidates were gathered from it before the SDP was
    /// generated, so the peer receives them embedded in the offer/answer).
    pub(super) socket: std::net::UdpSocket,
    /// TURN allocation when one was obtained: transmits sourced from the relayed
    /// address are wrapped for the relay; Data Indications are unwrapped.
    pub(super) turn: Option<crate::turn::TurnAllocation>,
    pub(super) stop: Arc<AtomicBool>,
    /// OS services: the host creates its injector and clipboard from this.
    pub(super) platform: Arc<dyn Platform>,
    /// Viewer: the session's current surface (plans/09 W2), let go when
    /// the link dies.
    pub(super) surface: Option<Arc<super::current::SurfaceCell>>,
    /// Viewer: the main video's gate. The link-trouble hide and the restore
    /// go through it, never straight to the surface (B4).
    pub(super) view: Option<Arc<super::active::SessionView>>,
    /// Per-session state shared with the media threads.
    pub(super) shared: Arc<Shared>,
    /// Host: where received files are written.
    pub(super) downloads_dir: PathBuf,
    /// Host: paste a received file into the window it was dropped on.
    pub(super) paste_dropped: bool,
    /// Host: the shared display's rectangle when it is not the primary one.
    pub(super) target_rect: Option<Rect>,
    /// Host: what starting a pop-out stream needs (D14).
    pub(super) host: Option<crate::media::host::HostCtx>,
}

/// Host: ask video stream `slot`'s encoder for an IDR (rate-limited there).
fn host_force_key(slot: u8, main: &Option<Arc<AtomicBool>>, shared: &Shared) {
    if slot == 0 {
        if let Some(fk) = main {
            fk.store(true, Ordering::SeqCst);
        }
    } else if let Some(p) = shared.host_streams.lock().popouts.get(&slot) {
        p.force_key.store(true, Ordering::SeqCst);
    }
}

/// Viewer: every open pop-out window's surface (D14).
fn popout_surfaces(shared: &Shared) -> Vec<Arc<dyn VideoSurface>> {
    shared
        .popouts
        .lock()
        .values()
        .map(|p| p.surface.clone())
        .collect()
}

/// Viewer: every open pop-out window's stream slot and surface (D14).
fn popout_slots(shared: &Shared) -> Vec<(u8, Arc<dyn VideoSurface>)> {
    shared
        .popouts
        .lock()
        .iter()
        .map(|(slot, p)| (*slot, p.surface.clone()))
        .collect()
}

/// Most video streams a session carries (main + pop-outs).
const MAX_STREAMS: usize = 1 + crate::media::host::MAX_POPOUTS as usize;

pub(super) fn transport_driver(d: Driver) {
    let Driver {
        rtc,
        mut pending,
        signal_rx,
        ctl_rx,
        frame_rx,
        video_tx,
        inject,
        cursor_rx,
        bulk_rx,
        clip_tx,
        channels,
        video_mids,
        force_key,
        bitrate,
        link: link_bw,
        ui,
        ended,
        end_tx,
        id,
        socket,
        mut turn,
        stop,
        platform,
        surface,
        view,
        shared,
        downloads_dir,
        paste_dropped,
        target_rect,
        host,
    } = d;
    use transport::{Inbound, Transport};

    // Liveness: end the session exactly once, however the loop exits (ICE dead,
    // consent lost, inactivity, error) — unless something ended it first.
    // Without this the controller sits on a dead session with no notification.
    let notify_dead = {
        let ended = ended.clone();
        let surface = surface.clone();
        let view = view.clone();
        let shared = shared.clone();
        move |reason: &str| {
            let first = ended
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            if first {
                // Take the native video surface down FIRST. It sits on top of the
                // web UI, so leaving it up means the user keeps staring at the
                // last decoded frame while the app thinks it is idle — the
                // "it just froze instead of disconnecting" report. Input capture
                // goes with it so a dead session can't still grab keystrokes.
                if let Some(cell) = &surface {
                    // Through the gate, so it is ordered after any show it queued.
                    if let Some(v) = &view {
                        v.retire();
                    }
                    // No more moves; input and drops stop.
                    cell.retire();
                }
                for s in popout_surfaces(&shared) {
                    s.hide();
                    s.set_input_sink(None);
                }
                // The engine ends it (events, teardown) — the flag is already
                // set, so it does not run twice.
                let _ = end_tx.send(crate::EndRequest {
                    id,
                    reason: crate::EndReason::LinkLost(reason.to_string()),
                    if_unanswered: None,
                });
            }
            first
        }
    };

    // Host side: injector for remote input, gated by the live control permission.
    // Created on this thread (backends may hold thread-bound OS resources). Not
    // being able to inject (e.g. macOS Accessibility not granted) leaves the
    // session view-only rather than failing it, and says why.
    let mut injector: Option<Box<dyn InputInjector>> = match inject {
        Some(_) => match platform.create_injector() {
            Ok(mut inj) => {
                inj.set_target_rect(target_rect);
                Some(inj)
            }
            Err(e) => {
                tracing::warn!("remote control unavailable: {e}");
                ui.send(crate::SessionEvent::Toast(format!(
                    "Remote control is unavailable on this computer: {e}"
                )));
                None
            }
        },
        None => None,
    };
    let mut was_control = true;

    // The socket was bound and its host candidates registered before the SDP was
    // generated (see `bind_and_gather`), so the peer already has our candidates.
    let mut tp = Transport::new(rtc);
    // Host: adopt the data-channel ids + video media-track mid created in
    // `begin_host`, and tell the BWE how high to probe (up to our max bitrate).
    if let Some(ch) = channels {
        tp.set_channels(ch);
    }
    if !video_mids.is_empty() {
        tp.set_video_mids(video_mids);
    }
    let host_side = frame_rx.is_some();
    if host_side {
        tp.set_desired_bitrate(DESIRED_BITRATE);
    }
    // Host: the desired bitrate last given to the BWE (see 2.7 below).
    let mut desired_applied = DESIRED_BITRATE;

    // ctl messages are QUEUED until the ctl channel opens (they're control
    // state like the initial Perm — losing them desyncs the session) and again
    // whenever SCTP's send buffer is full, in order (see `ctl_queue`). Video is a
    // media track: `send_video` no-ops until it's negotiated + connected, and a
    // keyframe is forced on connect so the first delivered frame is decodable.
    let mut ctl = super::ctl_queue::CtlQueue::default();
    let mut cursor_open = false;
    let mut bulk_open = false;
    // Host: the file transfer currently being received, if any, and where on
    // this screen the sender aimed it.
    let mut incoming: Option<crate::transfer::Incoming> = None;
    let mut incoming_drop_at: Option<(f64, f64)> = None;
    // Viewer: the host's last monitor announcement, re-sent to the UI when our
    // control permission changes (switching needs control).
    let mut remote_displays: Option<(
        Vec<protocol::RemoteDisplay>,
        String,
        Vec<protocol::PopoutStream>,
    )> = None;
    // Host: where pointer input maps (D14). The main stream's monitor rect (it
    // changes on a switch) and the stream the viewer last said its input is
    // over (`InputTarget`).
    let mut main_rect = target_rect;
    let mut input_slot: u8 = 0;

    // Liveness watchdog: if NOTHING arrives from the peer for this long, the
    // connection is dead even if ICE hasn't declared it yet. str0m's ICE consent
    // checks also emit Disconnected within a few seconds; this is the backstop.
    /// Backstop only. It must sit ABOVE the ping/pong grace below, or it would
    /// kill the session before the user ever sees the countdown — the app-level
    /// keepalive owns that decision because it can explain itself to the user.
    const DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(20);
    let mut last_rx = std::time::Instant::now();
    let mut connected = false; // becomes true on the first Connected event

    // Application-level ping/pong. UDP liveness (`last_rx`) counts STUN/TURN
    // keepalives too, so it can look healthy while the session itself is wedged.
    // Both sides ping; both sides enforce the grace period, so a dead link tears
    // BOTH ends down rather than leaving one staring at a frozen picture.
    const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    /// How long the peer may stay silent before the session is given up on.
    const PONG_GRACE: std::time::Duration = std::time::Duration::from_secs(15);
    /// Silence beyond this is reported to the user (with a countdown) while we
    /// keep waiting. Long enough not to fire on ordinary jitter, short enough
    /// that a frozen picture is explained rather than left a mystery.
    const TROUBLE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
    let mut last_ping_sent = std::time::Instant::now();
    // When the peer's ctl channel last delivered anything (a pong or any other
    // message: see `Inbound::Ctl`). The silence timers run from here.
    let mut last_pong = std::time::Instant::now();
    let mut ping_seq: u32 = 0;
    let mut link_trouble = false;
    let mut last_trouble_tick = std::time::Instant::now();

    // Viewer: NEVER hand the decoder a delta whose reference chain is broken.
    // str0m NACK-repairs most loss; when a gap survives its reorder window it
    // delivers the next frame with `contiguous=false`. From that instant every
    // delta is undecodable garbage (the "random black/smeared pixels") — drop
    // them all and request a keyframe (rate-limited) until an IDR arrives.
    // Per video stream (D14: pop-out windows have their own).
    let mut drop_till_key = [false; MAX_STREAMS];
    let mut last_pli = [std::time::Instant::now() - std::time::Duration::from_secs(5); MAX_STREAMS];

    // Periodic pipeline stats (both sides, every 5s) so field tests yield DATA:
    // host logs frames/IDRs sent + applied bitrate + last BWE estimate; viewer
    // logs frames received + gaps + frames dropped while awaiting an IDR.
    let mut last_stats = std::time::Instant::now();
    let mut stat_sent: u64 = 0;
    let mut stat_idr: u64 = 0;
    let mut stat_gaps: u64 = 0;
    let mut stat_dropped: u64 = 0;
    let mut stat_pli: u64 = 0;
    let mut last_est: u32 = 0;
    // V3: smooth the encoder target against BWE noise. Seeded from the initial
    // encoder bitrate so the first estimates don't lurch, and `last_bwe_at` gives
    // the filter the wall-clock gap between estimates (its EMA is time-based).
    let mut rate_filter = crate::tuning::RateFilter::new(
        bitrate
            .as_ref()
            .map(|b| b.load(Ordering::SeqCst))
            .unwrap_or(crate::tuning::TARGET_BITRATE),
    );
    let mut last_bwe_at = std::time::Instant::now();

    let mut buf = [0u8; 2048];
    let mut video_count: u64 = 0;
    // Test hook (`Shared::replay_video`): the main stream's delta frames since
    // its last keyframe, newest last, a few at most.
    #[cfg(feature = "test-hooks")]
    let mut recent_deltas: std::collections::VecDeque<Vec<u8>> = Default::default();
    while !stop.load(Ordering::SeqCst) {
        // Test hook: a link that dies right now, as a dead network would
        // make it (the loopback has no network to cut).
        #[cfg(feature = "test-hooks")]
        if shared.sever.load(Ordering::SeqCst) {
            notify_dead("severed by a test");
            break;
        }
        // Test hook: those frames once more, as late arrivals would come.
        #[cfg(feature = "test-hooks")]
        if shared.replay_video.swap(false, Ordering::SeqCst) {
            if let Some(tx) = &video_tx {
                for au in recent_deltas.iter() {
                    let _ = tx.send((au.clone(), false));
                    shared.replayed_video.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        // Liveness: once connected, no traffic for DEAD_AFTER ⇒ declare it dead.
        if connected && last_rx.elapsed() > DEAD_AFTER {
            tracing::warn!("transport: no data for {DEAD_AFTER:?} — declaring dead");
            notify_dead("no response from the peer");
            break;
        }
        // Application-level keepalive: ping on a timer, and give up if the peer
        // has not answered within the grace period. Best-effort Bye first so the
        // other end tears down promptly instead of waiting out its own timer.
        if connected && ctl.is_open() {
            if last_ping_sent.elapsed() >= PING_EVERY {
                last_ping_sent = std::time::Instant::now();
                ping_seq = ping_seq.wrapping_add(1);
                ctl.send(
                    serialize(&protocol::ControlMsg::Ping { seq: ping_seq }),
                    |b| tp.send_ctl(b),
                );
            }
            let silent = last_pong.elapsed();
            if silent > PONG_GRACE {
                tracing::warn!("transport: no pong for {PONG_GRACE:?} — declaring dead");
                ctl.send(serialize(&protocol::ControlMsg::Bye), |b| tp.send_ctl(b));
                notify_dead("the peer stopped responding");
                break;
            } else if silent >= TROUBLE_AFTER {
                if !link_trouble {
                    link_trouble = true;
                    tracing::warn!("transport: peer quiet — {PONG_GRACE:?} grace started");
                    // Stop presenting a stale frame while we wait. Without this
                    // the viewer sees a frozen picture that is indistinguishable
                    // from a working session, which is the whole complaint.
                    if let Some(v) = &view {
                        v.set_trouble(true);
                    }
                    for s in popout_surfaces(&shared) {
                        s.hide();
                    }
                }
                // Re-emit ~1/s so the UI can count down.
                if last_trouble_tick.elapsed() >= std::time::Duration::from_millis(900) {
                    last_trouble_tick = std::time::Instant::now();
                    let left = PONG_GRACE.saturating_sub(silent).as_secs() as u32;
                    ui.send(crate::SessionEvent::LinkTrouble { secs_left: left });
                }
            } else if link_trouble {
                // Answered again within the grace period — carry on.
                link_trouble = false;
                tracing::info!("transport: peer responded again — session continues");
                if let Some(v) = &view {
                    // Shown again only if it is still the active tab.
                    v.set_trouble(false);
                    // The stream may have advanced without us; get a clean start.
                    ctl.send(
                        serialize(&protocol::ControlMsg::KeyframeRequest { s: 0 }),
                        |b| tp.send_ctl(b),
                    );
                }
                // Not while the host's screen is blocked: the end of the
                // block shows them (K21).
                if !shared.host_block.blocked() {
                    for (slot, s) in popout_slots(&shared) {
                        s.show();
                        ctl.send(
                            serialize(&protocol::ControlMsg::KeyframeRequest { s: slot }),
                            |b| tp.send_ctl(b),
                        );
                    }
                }
                ui.send(crate::SessionEvent::LinkRestored);
            }
        }
        // Field-test telemetry: one INFO line per 5s per side.
        if connected && last_stats.elapsed() >= std::time::Duration::from_secs(5) {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = std::time::Instant::now();
            if host_side {
                tracing::info!(
                    "video/tx: {:.1} fps, {} idr, {} pli in, applied {} kbps, bwe {} kbps",
                    stat_sent as f64 / secs,
                    stat_idr,
                    stat_pli,
                    bitrate
                        .as_ref()
                        .map(|b| b.load(Ordering::SeqCst) / 1000)
                        .unwrap_or(0),
                    last_est / 1000,
                );
            } else {
                tracing::info!(
                    "video/rx: {:.1} fps, {} gap(s), {} dropped awaiting idr",
                    stat_sent as f64 / secs, // viewer: frames received this window
                    stat_gaps,
                    stat_dropped,
                );
            }
            stat_sent = 0;
            stat_idr = 0;
            stat_gaps = 0;
            stat_dropped = 0;
            stat_pli = 0;
        }
        // 0) Host: if control was just revoked (view-only), release any input we
        // are holding down so the host is never left with a stuck key/button.
        if let (Some(inj), Some(gate)) = (injector.as_mut(), inject.as_ref()) {
            let now = gate.load(Ordering::SeqCst);
            if was_control && !now {
                inj.release_all();
            }
            was_control = now;
        }
        // Host (D14): pop-out windows are a control privilege; revoking control
        // closes them (the viewer keeps the main window's monitor only).
        if let (Some(ctx), Some(gate)) = (host.as_ref(), inject.as_ref()) {
            if !gate.load(Ordering::SeqCst) && shared.host_streams.lock().close_all() {
                tracing::info!("control revoked — closing pop-out streams");
                crate::media::host::announce(ctx);
            }
        }
        // Host: the pop-out input was mapped onto is gone (closed by any
        // path) — fall back to the main monitor until the viewer says
        // otherwise, never leave input aimed at a monitor nobody sees.
        if host.is_some()
            && input_slot != 0
            && !shared.host_streams.lock().popouts.contains_key(&input_slot)
        {
            input_slot = 0;
            if let Some(inj) = injector.as_mut() {
                inj.set_target_rect(main_rect);
            }
        }
        // Host: a monitor switch took effect on the media thread — map input
        // onto the newly shared monitor from here on.
        let new_rect = shared.input_rect.lock().take();
        if let Some(rect) = new_rect {
            main_rect = rect;
            if input_slot == 0 {
                if let Some(inj) = injector.as_mut() {
                    inj.set_target_rect(rect);
                }
            }
        }

        // 1) Accept inbound answer/ICE relayed from signaling.
        while let Ok(data) = signal_rx.try_recv() {
            match data {
                SignalData::Answer { sdp } => {
                    if let (Some(p), Ok(ans)) = (
                        pending.take(),
                        str0m::change::SdpAnswer::from_sdp_string(&sdp),
                    ) {
                        let _ = tp.rtc_mut().sdp_api().accept_answer(p, ans);
                    }
                }
                SignalData::Ice { candidate } => {
                    if let Ok(cand) = str0m::Candidate::from_sdp_string(&candidate.candidate) {
                        tp.rtc_mut().add_remote_candidate(cand);
                    }
                }
                SignalData::Offer { .. } => {} // handled at session start
            }
        }

        // 2) Outbound control + encoded video from the media threads.
        // Whatever the transport refused last pass goes first (the peer may have
        // acknowledged since), then the new messages behind it.
        ctl.flush(|b| tp.send_ctl(b));
        while let Ok(bytes) = ctl_rx.try_recv() {
            ctl.send(bytes, |b| tp.send_ctl(b));
        }
        // Host: the session's screen state, if it changed (plans/10 U1, K13).
        // Only this thread takes changes, so they go out once each, in order;
        // queued like everything else until the channel opens.
        if let Some(h) = &host {
            if let Some(msg) = crate::media::host::pending_hstate(&shared, h.hstate) {
                ctl.send(msg, |b| tp.send_ctl(b));
            }
        }
        if let Some(frame_rx) = &frame_rx {
            while let Ok((slot, au, keyframe)) = frame_rx.try_recv() {
                // Write onto the RTP media track. str0m packetizes, paces, and
                // NACK-repairs; it no-ops until the track is connected. The encoder
                // bitrate is steered by the BWE back-off (Inbound::BweEstimate
                // below), not a hand-rolled send-queue heuristic.
                stat_sent += 1;
                if keyframe {
                    stat_idr += 1;
                }
                let _ = tp.send_video(slot, &au);
            }
        }
        // Host: cursor position updates on the cursor channel (§5a/§7). Stale
        // pre-open positions are worthless — drop, don't buffer.
        if let Some(cursor_rx) = &cursor_rx {
            while let Ok(bytes) = cursor_rx.try_recv() {
                if cursor_open {
                    let _ = tp.send_cursor(&bytes);
                }
            }
        }
        // Viewer: outbound file bytes, WITH BACKPRESSURE. Queueing a whole file
        // at once would hand SCTP megabytes to push as fast as it can, starving
        // the video stream for the entire transfer; stopping while the channel
        // already holds a backlog keeps the session usable while files move.
        if let Some(bulk_rx) = &bulk_rx {
            if bulk_open {
                const BULK_QUEUE_LIMIT: usize = 256 * 1024;
                while tp.bulk_buffered() < BULK_QUEUE_LIMIT {
                    match bulk_rx.try_recv() {
                        Ok(frame) => {
                            let _ = tp.send_bulk(&frame);
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // 2.5) TURN housekeeping: refresh the allocation before it expires.
        if let Some(alloc) = turn.as_mut() {
            alloc.tick(&socket);
        }

        // 2.6) Test hook: a forced bandwidth estimate, re-applied every tick so it
        // sticks (the real str0m estimate is ignored while it is set). Never
        // compiled into a shipping build.
        #[cfg(feature = "test-hooks")]
        {
            let forced = shared.bwe_inject.load(Ordering::Relaxed);
            if forced != 0 {
                let dt = last_bwe_at.elapsed().as_secs_f64();
                last_bwe_at = std::time::Instant::now();
                apply_bwe(forced, dt, &mut rate_filter, &bitrate, &link_bw);
            }
        }

        // 2.7) Host, background tab (P6): with the main stream paused and no
        // pop-out streaming, ask the BWE for no more than the floor, so it
        // does not keep probing (padding) the link up toward a rate nothing
        // will use; back to the full rate on resume or when a pop-out opens.
        if host_side {
            let idle = shared.video_paused.load(Ordering::SeqCst)
                && shared.host_streams.lock().popouts.is_empty();
            let want = if idle {
                crate::tuning::MIN_BITRATE
            } else {
                DESIRED_BITRATE
            };
            if want != desired_applied {
                tp.set_desired_bitrate(want);
                desired_applied = want;
            }
        }

        // 3) Drive str0m: emit transmits, handle timeouts, surface events.
        match tp.poll_output() {
            Ok(str0m::Output::Transmit(t)) => {
                // Transmits sourced from the relayed address go via the TURN
                // server (Send Indication); everything else is direct UDP.
                let via_relay = turn.as_ref().is_some_and(|alloc| t.source == alloc.relayed);
                if via_relay {
                    if let Some(alloc) = turn.as_mut() {
                        alloc.send_via_relay(&socket, t.destination, &t.contents);
                    }
                } else {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
            }
            Ok(str0m::Output::Timeout(_)) => {
                // 4) Drain ALL pending UDP into str0m (bounded per cycle), then
                // advance time if there was nothing. One-packet-per-poll-cycle
                // couldn't keep up at video rates (~300 pkts/s + NACK + TWCC
                // feedback), which distorted the BWE's arrival timing and delayed
                // loss repair. The bound keeps transmits flowing under flood.
                let mut received = 0u32;
                while received < 64 {
                    match socket.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            received += 1;
                            // Liveness: any datagram from the peer proves the link.
                            last_rx = std::time::Instant::now();
                            // Packets from the TURN server: unwrap Data Indications
                            // (relayed peer traffic, reported as arriving on the
                            // relayed address) and consume control responses.
                            let is_turn_server =
                                turn.as_ref().is_some_and(|alloc| from == alloc.server);
                            if is_turn_server {
                                let alloc = turn.as_mut().expect("checked above");
                                if let Some((peer, data)) = alloc.handle_server_packet(&buf[..n]) {
                                    let recv = str0m::net::Receive::new(
                                        str0m::net::Protocol::Udp,
                                        peer,
                                        alloc.relayed,
                                        &data,
                                    );
                                    if let Ok(recv) = recv {
                                        let _ = tp.handle_input(str0m::Input::Receive(
                                            std::time::Instant::now(),
                                            recv,
                                        ));
                                    }
                                }
                            } else if let Ok(local) = socket.local_addr() {
                                let recv = str0m::net::Receive::new(
                                    str0m::net::Protocol::Udp,
                                    from,
                                    local,
                                    &buf[..n],
                                );
                                if let Ok(recv) = recv {
                                    let _ = tp.handle_input(str0m::Input::Receive(
                                        std::time::Instant::now(),
                                        recv,
                                    ));
                                }
                            }
                        }
                        Err(_) => break, // WouldBlock — nothing pending
                    }
                }
                if received == 0 {
                    let _ = tp.handle_input(str0m::Input::Timeout(std::time::Instant::now()));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            Ok(str0m::Output::Event(ev)) => {
                if let Some(inbound) = tp.on_event(ev) {
                    match inbound {
                        Inbound::Connected => {
                            connected = true;
                            last_rx = std::time::Instant::now();
                            last_pong = std::time::Instant::now(); // start the grace period here
                                                                   // Host: the viewer's decoder has no reference yet, and
                                                                   // anything written before connect was dropped — force an
                                                                   // IDR so the first *delivered* frame is decodable.
                            if let Some(fk) = &force_key {
                                fk.store(true, Ordering::SeqCst);
                            }
                            tracing::info!(
                                "transport connected ({})",
                                if inject.is_some() { "host" } else { "viewer" }
                            );
                        }
                        Inbound::ChannelOpen(_, label) => {
                            tracing::info!("data channel open: {label}");
                            match label.as_str() {
                                "ctl" => {
                                    ctl.open();
                                    // Ping/pong only starts now — don't count the
                                    // channel-setup time against the grace period.
                                    last_pong = std::time::Instant::now();
                                    ctl.flush(|b| tp.send_ctl(b));
                                }
                                "cursor" => cursor_open = true,
                                "bulk" => bulk_open = true,
                                _ => {} // "init" — SDP bootstrap channel, unused
                            }
                        }
                        Inbound::Video {
                            slot,
                            data,
                            keyframe,
                            contiguous,
                        } => {
                            let i = usize::from(slot);
                            if i >= MAX_STREAMS {
                                continue;
                            }
                            video_count += 1;
                            stat_sent += 1;
                            if video_count <= 5 {
                                tracing::info!(
                                    "viewer: received video frame #{video_count} ({} bytes, keyframe={keyframe})",
                                    data.len(),
                                );
                            }
                            // CORRECTNESS GATE: a non-contiguous frame means loss
                            // survived str0m's NACK + reorder window, so every delta
                            // from here is undecodable garbage (black/smeared
                            // blocks). Drop them ALL until an IDR arrives; keep
                            // asking for one (rate-limited) on both paths — native
                            // RTCP PLI and the reliable ctl channel.
                            if !contiguous && !keyframe && !drop_till_key[i] {
                                stat_gaps += 1;
                                drop_till_key[i] = true;
                            }
                            if drop_till_key[i] {
                                if keyframe {
                                    drop_till_key[i] = false; // clean restart point
                                } else {
                                    stat_dropped += 1;
                                    if last_pli[i].elapsed()
                                        >= std::time::Duration::from_millis(500)
                                    {
                                        tp.request_keyframe(slot);
                                        ctl.send(
                                            serialize(&protocol::ControlMsg::KeyframeRequest {
                                                s: slot,
                                            }),
                                            |b| tp.send_ctl(b),
                                        );
                                        last_pli[i] = std::time::Instant::now();
                                    }
                                    continue; // NEVER feed a broken delta
                                }
                            }
                            if slot == 0 {
                                // Test hook: remember the deltas since the
                                // last keyframe (see `recent_deltas`).
                                #[cfg(feature = "test-hooks")]
                                if keyframe {
                                    recent_deltas.clear();
                                } else {
                                    if recent_deltas.len() == 8 {
                                        recent_deltas.pop_front();
                                    }
                                    recent_deltas.push_back(data.clone());
                                }
                                if let Some(tx) = &video_tx {
                                    let _ = tx.send((data, keyframe));
                                }
                            } else {
                                // A pop-out stream: to its window, if open yet
                                // (it asks for an IDR when it opens).
                                let tx =
                                    shared.popouts.lock().get(&slot).map(|p| p.video_tx.clone());
                                if let Some(tx) = tx {
                                    let _ = tx.send((data, keyframe));
                                }
                            }
                        }
                        Inbound::KeyframeRequest(slot) => {
                            // Host: the peer (or str0m on its behalf) asked for a
                            // keyframe over RTCP. Counted, because this path was
                            // invisible in the field logs while the ctl-channel
                            // requests were counted — leaving most IDRs
                            // unattributed. The host rate-limits what it actually
                            // emits (see `host_media_loop`).
                            stat_pli += 1;
                            host_force_key(slot, &force_key, &shared);
                        }
                        Inbound::BweEstimate(bps) => {
                            last_est = bps;
                            // Smooth the encoder bitrate (V3 `RateFilter`) and record
                            // the raw estimate from the one value. While a test is
                            // holding the link at a fixed value, the real estimate is
                            // ignored so the scenario is deterministic (see
                            // `apply_bwe`).
                            if !bwe_overridden(&shared) {
                                let dt = last_bwe_at.elapsed().as_secs_f64();
                                last_bwe_at = std::time::Instant::now();
                                apply_bwe(bps, dt, &mut rate_filter, &bitrate, &link_bw);
                            }
                        }
                        Inbound::Ctl(bytes) => {
                            // Liveness: ANY ctl message proves the peer's session
                            // is alive, not only a pong. The peer's ctl is one
                            // ordered queue, so its pong can wait behind a cursor
                            // image backlog (D9: up to MBs on a slow uplink) while
                            // the parts ahead of it keep arriving; counting only
                            // pongs would show "link trouble", even end the
                            // session, while everything flows.
                            last_pong = std::time::Instant::now();
                            // Control messages both sides understand, handled once
                            // up front. Anything that isn't one falls through to
                            // the host's input-injection path below.
                            match serde_json::from_slice::<protocol::ControlMsg>(&bytes) {
                                // Answer probes (the reply proves us alive to them).
                                Ok(protocol::ControlMsg::Ping { seq }) => {
                                    ctl.send(serialize(&protocol::ControlMsg::Pong { seq }), |b| {
                                        tp.send_ctl(b)
                                    });
                                    continue;
                                }
                                Ok(protocol::ControlMsg::Pong { .. }) => continue,
                                // A viewer keyframe request restarts the stream
                                // (decoder never started / lost the keyframe). NOT
                                // gated on control — view-only viewers need it too.
                                Ok(protocol::ControlMsg::KeyframeRequest { s }) if host_side => {
                                    tracing::info!("viewer requested keyframe (stream {s})");
                                    host_force_key(s, &force_key, &shared);
                                    continue;
                                }
                                // Background tab (P6): the main stream stops until
                                // the resume.
                                Ok(protocol::ControlMsg::VideoPause) if host_side => {
                                    tracing::info!(
                                        "viewer paused the main stream (background tab)"
                                    );
                                    shared.video_paused.store(true, Ordering::SeqCst);
                                    continue;
                                }
                                Ok(protocol::ControlMsg::VideoResume) if host_side => {
                                    tracing::info!("viewer resumed the main stream");
                                    // No keyframe needed: the viewer decoded every
                                    // frame up to the pause, so the stream goes on
                                    // from there (and an IDR burst is the costliest
                                    // frame there is).
                                    shared.video_paused.store(false, Ordering::SeqCst);
                                    continue;
                                }
                                // The viewer told us its display size — encode to
                                // exactly that so it presents 1:1 (no resampling
                                // blur) and we don't spend bits on unseen pixels.
                                Ok(protocol::ControlMsg::ViewSize { width, height, s })
                                    if host_side =>
                                {
                                    let v = ((width as u64) << 32) | height as u64;
                                    if s == 0 {
                                        shared.view_size.store(v, Ordering::SeqCst);
                                    } else if let Some(p) =
                                        shared.host_streams.lock().popouts.get(&s)
                                    {
                                        p.view_size.store(v, Ordering::SeqCst);
                                    }
                                    continue;
                                }
                                // Clipboard text from the peer. On the host this
                                // is gated on the live control permission, same
                                // as input and file receipt.
                                Ok(protocol::ControlMsg::Clipboard { text }) => {
                                    let allowed = inject
                                        .as_ref()
                                        .map(|gate| gate.load(Ordering::SeqCst))
                                        .unwrap_or(true); // viewer: no gate
                                    if allowed && text.len() <= platform_api::MAX_TEXT_BYTES {
                                        let _ = clip_tx.send(text);
                                    }
                                    continue;
                                }
                                // Viewer: the host changed our live permission
                                // (§4.2). Track control (task D10) so the cursor
                                // sprite follows the LOCAL pointer only while
                                // controlling, and hide or restore the OS cursor
                                // to match — a revoke must bring it back.
                                Ok(protocol::ControlMsg::Perm { value }) if !host_side => {
                                    let control = value == protocol::Permission::Control;
                                    shared.control.store(control, Ordering::SeqCst);
                                    if let Some(cell) = &surface {
                                        cell.set_cursor_control(control);
                                    }
                                    for p in shared.popouts.lock().values() {
                                        p.shared.control.store(control, Ordering::SeqCst);
                                        p.surface.set_cursor_control(control);
                                    }
                                    // The monitor picker follows control.
                                    if let Some((list, current, popouts)) = &remote_displays {
                                        ui.send(crate::SessionEvent::Displays {
                                            list: list.clone(),
                                            current: current.clone(),
                                            can_switch: control,
                                            popouts: popouts.clone(),
                                        });
                                    }
                                    continue;
                                }
                                // Viewer: the host's monitors / the one shown now,
                                // and the pop-out streams (the app opens and
                                // closes windows to match).
                                Ok(protocol::ControlMsg::Displays {
                                    list,
                                    current,
                                    popouts,
                                }) => {
                                    if !host_side {
                                        ui.send(crate::SessionEvent::Displays {
                                            list: list.clone(),
                                            current: current.clone(),
                                            can_switch: shared.control.load(Ordering::SeqCst),
                                            popouts: popouts.clone(),
                                        });
                                        remote_displays = Some((list, current, popouts));
                                    }
                                    continue;
                                }
                                // Host (D14): the viewer wants a monitor in a
                                // separate window too. Control only, as for a
                                // switch. Always answered with `displays`.
                                Ok(protocol::ControlMsg::Popout { id }) => {
                                    if let Some(ctx) = &host {
                                        let allowed = inject
                                            .as_ref()
                                            .is_some_and(|gate| gate.load(Ordering::SeqCst));
                                        if allowed {
                                            crate::media::host::start_popout(ctx, &id);
                                        } else {
                                            tracing::warn!(
                                                "ignored a pop-out request from a view-only viewer"
                                            );
                                        }
                                        crate::media::host::announce(ctx);
                                    }
                                    continue;
                                }
                                // Host: the viewer closed that pop-out window.
                                Ok(protocol::ControlMsg::PopoutClose { s }) => {
                                    if let Some(ctx) = &host {
                                        let removed = shared.host_streams.lock().popouts.remove(&s);
                                        if let Some(p) = removed {
                                            tracing::info!("host: pop-out window {s} closed");
                                            p.stop.store(true, Ordering::SeqCst);
                                            crate::media::host::announce(ctx);
                                        }
                                        if input_slot == s {
                                            input_slot = 0;
                                            if let Some(inj) = injector.as_mut() {
                                                inj.set_target_rect(main_rect);
                                            }
                                        }
                                    }
                                    continue;
                                }
                                // Host: the pointer input that follows is over
                                // stream `s`'s window — map it onto that monitor.
                                Ok(protocol::ControlMsg::InputTarget { s }) => {
                                    if host_side {
                                        let rect = if s == 0 {
                                            Some(main_rect)
                                        } else {
                                            let id = shared
                                                .host_streams
                                                .lock()
                                                .popouts
                                                .get(&s)
                                                .map(|p| p.id.clone());
                                            id.and_then(|id| {
                                                platform.displays().into_iter().find(|d| d.id == id)
                                            })
                                            .map(|d| super::input_rect_for(&d))
                                        };
                                        // An unknown stream (closed meanwhile)
                                        // leaves the mapping alone.
                                        if let Some(rect) = rect {
                                            input_slot = s;
                                            if let Some(inj) = injector.as_mut() {
                                                inj.set_target_rect(rect);
                                            }
                                        }
                                    }
                                    continue;
                                }
                                // Host: the viewer asks to see another monitor.
                                // Gated on the live CONTROL permission: a
                                // view-only peer sees only what the host chose
                                // to share. The media thread validates the id
                                // and always answers with a fresh `displays`.
                                Ok(protocol::ControlMsg::SelectDisplay { id }) => {
                                    if host_side {
                                        let allowed = inject
                                            .as_ref()
                                            .is_some_and(|gate| gate.load(Ordering::SeqCst));
                                        if allowed {
                                            tracing::info!("viewer asked to share monitor {id:?}");
                                            *shared.display_req.lock() = Some(id);
                                        } else {
                                            tracing::warn!(
                                                "ignored a monitor switch from a view-only viewer"
                                            );
                                        }
                                    }
                                    continue;
                                }
                                // Viewer: what the host's screen is (plans/10
                                // U1, 10a §3.3). While it is blocked nothing
                                // shows its picture — neither the main video
                                // (the gate) nor a pop-out — and no frame from
                                // before is repainted after (K21,
                                // `Shared::host_block`). The UI shows why.
                                Ok(protocol::ControlMsg::HostState { screen, kind, why }) => {
                                    if !host_side {
                                        let blocked = screen == protocol::HostScreen::Blocked;
                                        // First the record the present loops
                                        // read, then the gate: a show the gate
                                        // queues finds the held frame dropped.
                                        let changed = shared.host_block.set(blocked);
                                        if let Some(v) = &view {
                                            v.set_host_blocked(blocked);
                                        }
                                        if changed && blocked {
                                            for s in popout_surfaces(&shared) {
                                                s.hide();
                                            }
                                        } else if changed && !link_trouble {
                                            // Back: show the pop-outs (link
                                            // trouble keeps them hidden; its
                                            // end shows them) and ask for
                                            // fresh pictures, as the held
                                            // ones are gone.
                                            for (slot, s) in popout_slots(&shared) {
                                                s.show();
                                                ctl.send(
                                                    serialize(
                                                        &protocol::ControlMsg::KeyframeRequest {
                                                            s: slot,
                                                        },
                                                    ),
                                                    |b| tp.send_ctl(b),
                                                );
                                            }
                                        }
                                        if changed {
                                            tracing::info!(
                                                "viewer: the host's screen is {}",
                                                screen.as_str()
                                            );
                                        }
                                        ui.send(crate::SessionEvent::HostScreen {
                                            screen,
                                            kind,
                                            why,
                                        });
                                    }
                                    continue;
                                }
                                // Viewer: a part of the host's cursor image
                                // (D9). Reassembled and cached here; the media
                                // thread picks it up when a position names it.
                                // Never input, so the host side drops it too.
                                Ok(protocol::ControlMsg::CursorShape(part)) => {
                                    if !host_side {
                                        shared.cursor_images.lock().push(part);
                                    }
                                    continue;
                                }
                                // A clean goodbye from either end.
                                Ok(protocol::ControlMsg::Bye) => {
                                    notify_dead("the other side ended the session");
                                    break;
                                }
                                _ => {}
                            }
                            // Host: anything else on ctl is remote input — inject
                            // it, gated on the current control permission (§4.1).
                            if let (Some(inj), Some(gate)) = (injector.as_mut(), inject.as_ref()) {
                                if gate.load(Ordering::SeqCst) {
                                    if let Ok(msg) =
                                        serde_json::from_slice::<protocol::InputMsg>(&bytes)
                                    {
                                        inj.dispatch(&msg);
                                    }
                                }
                            }
                        }
                        Inbound::Bulk(bytes) => {
                            // Host: an inbound file. Writing to this machine's
                            // disk is at least as privileged as injecting input,
                            // so it needs the SAME live control permission — a
                            // view-only peer cannot drop files here.
                            let allowed = inject
                                .as_ref()
                                .is_some_and(|gate| gate.load(Ordering::SeqCst));
                            if !allowed {
                                // Delete the partial file: `Incoming` has no Drop,
                                // so just dropping it would leave a truncated file
                                // in the user's Downloads.
                                if let Some(partial) = incoming.take() {
                                    tracing::warn!("file transfer aborted: control revoked");
                                    partial.abort();
                                }
                                continue;
                            }
                            let Some(frame) = protocol::BulkFrame::decode(&bytes) else {
                                continue; // malformed — ignore, never trust it
                            };
                            match frame {
                                protocol::BulkFrame::Begin(meta) => {
                                    // A new Begin supersedes any transfer in
                                    // flight; drop that partial file first.
                                    if let Some(prev) = incoming.take() {
                                        prev.abort();
                                    }
                                    let dir = downloads_dir.clone();
                                    incoming_drop_at = meta.drop_at;
                                    match crate::transfer::Incoming::begin(&meta, &dir) {
                                        Ok(inc) => {
                                            tracing::info!(
                                                "receiving {} ({} bytes)",
                                                meta.name,
                                                meta.size
                                            );
                                            ui.send(crate::SessionEvent::Toast(format!(
                                                "Receiving {}…",
                                                meta.name
                                            )));
                                            incoming = Some(inc);
                                        }
                                        Err(e) => {
                                            tracing::warn!("refused file {}: {e}", meta.name);
                                            ui.send(crate::SessionEvent::Toast(format!(
                                                "Refused file: {e}"
                                            )));
                                        }
                                    }
                                }
                                protocol::BulkFrame::Chunk(data) => {
                                    if let Some(inc) = incoming.as_mut() {
                                        if let Err(e) = inc.write(&data) {
                                            tracing::warn!("file transfer failed: {e}");
                                            ui.send(crate::SessionEvent::Toast(format!(
                                                "Transfer failed: {e}"
                                            )));
                                            if let Some(bad) = incoming.take() {
                                                bad.abort();
                                            }
                                        }
                                    }
                                }
                                protocol::BulkFrame::End => {
                                    if let Some(inc) = incoming.take() {
                                        match inc.finish() {
                                            Ok(path) => {
                                                tracing::info!("received file {path:?}");
                                                let name = path
                                                    .file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy()
                                                    .into_owned();
                                                // The last state the host loop saw (10a §3.3). Read
                                                // into a local first: as an argument, the guard would
                                                // live until the call returns, holding the session's
                                                // screen lock (a leaf lock every capture thread takes)
                                                // across the clipboard write, the sleep and the paste.
                                                let screen_normal =
                                                    shared.host_screen.lock().state()
                                                        == platform_api::ScreenState::Normal;
                                                let pasted = super::files::deliver_received_file(
                                                    &path,
                                                    incoming_drop_at.take(),
                                                    // Coerce inside the closure: coercions do
                                                    // not apply through `Option`.
                                                    injector.as_mut().map(|b| {
                                                        &mut **b as &mut dyn InputInjector
                                                    }),
                                                    platform.as_ref(),
                                                    paste_dropped,
                                                    screen_normal,
                                                );
                                                ui.send(crate::SessionEvent::Toast(if pasted {
                                                    format!("Pasted {name}")
                                                } else {
                                                    format!(
                                                        "{name} is on the clipboard (also saved to {})",
                                                        downloads_dir.display()
                                                    )
                                                }));
                                            }
                                            Err(e) => {
                                                ui.send(crate::SessionEvent::Toast(format!(
                                                    "Transfer failed: {e}"
                                                )));
                                            }
                                        }
                                    }
                                }
                                protocol::BulkFrame::Abort(reason) => {
                                    if let Some(inc) = incoming.take() {
                                        inc.abort();
                                    }
                                    tracing::warn!("sender aborted transfer: {reason}");
                                    ui.send(crate::SessionEvent::Toast(
                                        "Transfer cancelled by sender".into(),
                                    ));
                                }
                            }
                        }
                        Inbound::Cursor(bytes) => {
                            // Viewer: update the sprite position (§7) and which
                            // of the host's images to draw there (D9).
                            if let Ok(protocol::ControlMsg::Cursor {
                                x,
                                y,
                                visible,
                                shape,
                                s,
                            }) = serde_json::from_slice::<protocol::ControlMsg>(&bytes)
                            {
                                let pointer = Some(super::ViewerPointer {
                                    x,
                                    y,
                                    visible,
                                    shape,
                                });
                                if s == 0 {
                                    *shared.cursor.lock() = pointer;
                                } else if let Some(p) = shared.popouts.lock().get(&s) {
                                    *p.shared.cursor.lock() = pointer;
                                }
                            }
                        }
                        Inbound::Disconnected => {
                            tracing::info!("transport disconnected");
                            notify_dead("the connection dropped");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("transport poll error: {e}");
                notify_dead("transport error");
                break;
            }
        }
    }

    // Local teardown (the user hit Disconnect): say goodbye on the DATA channel
    // as well. `Engine::end_session` announces over signaling, which is useless
    // when the signaling socket is itself the thing that broke — the peer would
    // then sit out the full grace period instead of dropping immediately.
    if stop.load(Ordering::SeqCst) && ctl.is_open() && connected {
        // Nothing still waiting matters once we leave (a cursor image backlog
        // would otherwise keep the Bye out of the 250 ms window below).
        ctl.clear();
        ctl.send(serialize(&protocol::ControlMsg::Bye), |b| tp.send_ctl(b));
        // The loop is over, so nothing else will flush str0m's queue — pump it
        // briefly by hand, bounded so teardown can't hang on a dead socket.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            match tp.poll_output() {
                Ok(str0m::Output::Transmit(t)) => {
                    let via_relay = turn.as_ref().is_some_and(|a| t.source == a.relayed);
                    if via_relay {
                        if let Some(alloc) = turn.as_mut() {
                            alloc.send_via_relay(&socket, t.destination, &t.contents);
                        }
                    } else {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                }
                // Nothing queued right now — the Bye is on the wire.
                _ => break,
            }
        }
    }

    // However the transport ended, the rest of the session stops with it, so
    // nothing keeps running behind a dead connection (a no-op when the session
    // was already being stopped).
    stop.store(true, Ordering::SeqCst);
    // Host: whatever ends the session (disconnect, teardown, transport error),
    // never leave remotely-injected keys/buttons held down on this machine.
    if let Some(inj) = injector.as_mut() {
        inj.release_all();
    }
}
