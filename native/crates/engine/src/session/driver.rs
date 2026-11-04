//! The transport driver: one thread per session that owns the str0m `Rtc`, the
//! UDP socket and the optional TURN allocation, and runs the sans-IO loop —
//! liveness, keyframe gating, bandwidth estimation, control/input/clipboard/file
//! routing. OS access goes through `platform_api` only.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use parking_lot::Mutex;
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
    /// Host: encoded AUs to send on the video channel.
    pub(super) frame_rx: Option<Receiver<(Vec<u8>, bool)>>,
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
    /// Host: the video RTP media track's mid (from `add_media`). Viewer learns
    /// its own from `Event::MediaAdded`, so this is `None` there.
    pub(super) video_mid: Option<str0m::media::Mid>,
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
    /// Notify the UI + reset the role when the transport dies unexpectedly (the
    /// driver thread has no `&Engine`). This is the fix for "you get disconnected
    /// but the controller thinks it's still alive with no notification."
    pub(super) ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    pub(super) role: Arc<Mutex<crate::Role>>,
    /// The bound UDP socket (candidates were gathered from it before the SDP was
    /// generated, so the peer receives them embedded in the offer/answer).
    pub(super) socket: std::net::UdpSocket,
    /// TURN allocation when one was obtained: transmits sourced from the relayed
    /// address are wrapped for the relay; Data Indications are unwrapped.
    pub(super) turn: Option<crate::turn::TurnAllocation>,
    pub(super) stop: Arc<AtomicBool>,
    /// OS services: the host creates its injector and clipboard from this.
    pub(super) platform: Arc<dyn Platform>,
    /// Viewer: the video surface (hidden on link trouble / death).
    pub(super) surface: Option<Arc<dyn VideoSurface>>,
    /// Per-session state shared with the media threads.
    pub(super) shared: Arc<Shared>,
    /// Host: where received files are written.
    pub(super) downloads_dir: PathBuf,
    /// Host: paste a received file into the window it was dropped on.
    pub(super) paste_dropped: bool,
    /// Host: the shared display's rectangle when it is not the primary one.
    pub(super) target_rect: Option<Rect>,
}

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
        video_mid,
        force_key,
        bitrate,
        link: link_bw,
        ui,
        role,
        socket,
        mut turn,
        stop,
        platform,
        surface,
        shared,
        downloads_dir,
        paste_dropped,
        target_rect,
    } = d;
    use transport::{Inbound, Transport};

    // Liveness: report the disconnect to the UI and reset the role exactly once,
    // however the loop exits (ICE dead, consent lost, inactivity, error). Without
    // this the controller sits on a dead session with no notification.
    let notify_dead = {
        let ui = ui.clone();
        let role = role.clone();
        let surface = surface.clone();
        move |reason: &str| {
            let was_active = !matches!(&*role.lock(), crate::Role::Idle);
            if was_active {
                *role.lock() = crate::Role::Idle;
                // Take the native video surface down FIRST. It sits on top of the
                // web UI, so leaving it up means the user keeps staring at the
                // last decoded frame while the app thinks it is idle — the
                // "it just froze instead of disconnecting" report. Input capture
                // goes with it so a dead session can't still grab keystrokes.
                if let Some(s) = &surface {
                    s.hide();
                    s.set_input_sink(None);
                    s.set_drop_sink(None);
                }
                let _ = ui.send(crate::UiEvent::Toast(format!("Disconnected: {reason}")));
                let _ = ui.send(crate::UiEvent::RoleChanged(crate::Role::Idle));
            }
            was_active
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
                let _ = ui.send(crate::UiEvent::Toast(format!(
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
    if let Some(mid) = video_mid {
        tp.set_video_mid(mid);
    }
    let host_side = frame_rx.is_some();
    if host_side {
        tp.set_desired_bitrate(DESIRED_BITRATE);
    }

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
    let mut drop_till_key = false;
    let mut last_pli = std::time::Instant::now() - std::time::Duration::from_secs(5);

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
    while !stop.load(Ordering::SeqCst) {
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
                    if let Some(s) = &surface {
                        s.hide();
                    }
                }
                // Re-emit ~1/s so the UI can count down.
                if last_trouble_tick.elapsed() >= std::time::Duration::from_millis(900) {
                    last_trouble_tick = std::time::Instant::now();
                    let left = PONG_GRACE.saturating_sub(silent).as_secs() as u32;
                    let _ = ui.send(crate::UiEvent::LinkTrouble { secs_left: left });
                }
            } else if link_trouble {
                // Answered again within the grace period — carry on.
                link_trouble = false;
                tracing::info!("transport: peer responded again — session continues");
                if let Some(s) = &surface {
                    s.show();
                    // The stream may have advanced without us; get a clean start.
                    ctl.send(serialize(&protocol::ControlMsg::KeyframeRequest), |b| {
                        tp.send_ctl(b)
                    });
                }
                let _ = ui.send(crate::UiEvent::LinkRestored);
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
        if let Some(frame_rx) = &frame_rx {
            while let Ok((au, keyframe)) = frame_rx.try_recv() {
                // Write onto the RTP media track. str0m packetizes, paces, and
                // NACK-repairs; it no-ops until the track is connected. The encoder
                // bitrate is steered by the BWE back-off (Inbound::BweEstimate
                // below), not a hand-rolled send-queue heuristic.
                stat_sent += 1;
                if keyframe {
                    stat_idr += 1;
                }
                let _ = tp.send_video(&au);
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
                            data,
                            keyframe,
                            contiguous,
                        } => {
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
                            if !contiguous && !keyframe && !drop_till_key {
                                stat_gaps += 1;
                                drop_till_key = true;
                            }
                            if drop_till_key {
                                if keyframe {
                                    drop_till_key = false; // clean restart point
                                } else {
                                    stat_dropped += 1;
                                    if last_pli.elapsed() >= std::time::Duration::from_millis(500) {
                                        tp.request_keyframe();
                                        ctl.send(
                                            serialize(&protocol::ControlMsg::KeyframeRequest),
                                            |b| tp.send_ctl(b),
                                        );
                                        last_pli = std::time::Instant::now();
                                    }
                                    continue; // NEVER feed a broken delta
                                }
                            }
                            if let Some(tx) = &video_tx {
                                let _ = tx.send((data, keyframe));
                            }
                        }
                        Inbound::KeyframeRequest => {
                            // Host: the peer (or str0m on its behalf) asked for a
                            // keyframe over RTCP. Counted, because this path was
                            // invisible in the field logs while the ctl-channel
                            // requests were counted — leaving most IDRs
                            // unattributed. The host rate-limits what it actually
                            // emits (see `host_media_loop`).
                            stat_pli += 1;
                            if let Some(fk) = &force_key {
                                fk.store(true, Ordering::SeqCst);
                            }
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
                                Ok(protocol::ControlMsg::KeyframeRequest) if host_side => {
                                    tracing::info!("viewer requested keyframe");
                                    if let Some(fk) = &force_key {
                                        fk.store(true, Ordering::SeqCst);
                                    }
                                    continue;
                                }
                                // The viewer told us its display size — encode to
                                // exactly that so it presents 1:1 (no resampling
                                // blur) and we don't spend bits on unseen pixels.
                                Ok(protocol::ControlMsg::ViewSize { width, height })
                                    if host_side =>
                                {
                                    shared.view_size.store(
                                        ((width as u64) << 32) | height as u64,
                                        Ordering::SeqCst,
                                    );
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
                                    if let Some(s) = &surface {
                                        s.set_cursor_control(control);
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
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
                                                "Receiving {}…",
                                                meta.name
                                            )));
                                            incoming = Some(inc);
                                        }
                                        Err(e) => {
                                            tracing::warn!("refused file {}: {e}", meta.name);
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
                                                "Refused file: {e}"
                                            )));
                                        }
                                    }
                                }
                                protocol::BulkFrame::Chunk(data) => {
                                    if let Some(inc) = incoming.as_mut() {
                                        if let Err(e) = inc.write(&data) {
                                            tracing::warn!("file transfer failed: {e}");
                                            let _ = ui.send(crate::UiEvent::Toast(format!(
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
                                                );
                                                let _ = ui.send(crate::UiEvent::Toast(if pasted {
                                                    format!("Pasted {name}")
                                                } else {
                                                    format!(
                                                        "{name} is on the clipboard (also saved to {})",
                                                        downloads_dir.display()
                                                    )
                                                }));
                                            }
                                            Err(e) => {
                                                let _ = ui.send(crate::UiEvent::Toast(format!(
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
                                    let _ = ui.send(crate::UiEvent::Toast(
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
                            }) = serde_json::from_slice::<protocol::ControlMsg>(&bytes)
                            {
                                *shared.cursor.lock() = Some(super::ViewerPointer {
                                    x,
                                    y,
                                    visible,
                                    shape,
                                });
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

    // If the transport died (role still active), also stop the session's other
    // threads so nothing keeps running behind a dead connection.
    if !matches!(&*role.lock(), crate::Role::Idle) {
        stop.store(true, Ordering::SeqCst);
    }
    // Host: whatever ends the session (disconnect, teardown, transport error),
    // never leave remotely-injected keys/buttons held down on this machine.
    if let Some(inj) = injector.as_mut() {
        inj.release_all();
    }
}
