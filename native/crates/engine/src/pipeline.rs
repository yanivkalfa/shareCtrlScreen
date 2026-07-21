//! Windows media + transport pipeline (Plan 04 §2 architecture, §5–§7). Wires
//! `capture`→`codec`→`transport` on the host and `transport`→`codec`→`render`
//! on the viewer, plus `input` injection. str0m owns the sans-IO WebRTC loop on
//! a dedicated thread; the COM-bound media stages each run on their own thread
//! with their own D3D11 device (COM interfaces are not `Send`), communicating
//! over byte channels.
//!
//! This module is the integration surface for the §12 latency smoke-test
//! (`capture → encode → transport → decode → render`), the single go/no-go for
//! the whole native-rewrite premise; the fine timing of the str0m↔UDP driver and
//! the encoder event pump are validated on target hardware there.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use protocol::{ControlMsg, Permission, SignalData};

use crate::Engine;

/// The viewer's native video HWND (Option A child window, §7), set by the app.
static RENDER_HWND: AtomicIsize = AtomicIsize::new(0);

/// The single active session (one at a time, contract §1).
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// The app calls this once its native video child window exists.
pub fn set_render_target(hwnd: isize) {
    RENDER_HWND.store(hwnd, Ordering::SeqCst);
}

struct Session {
    stop: Arc<AtomicBool>,
    /// Feed inbound answer/ICE from signaling into the transport thread.
    signal_tx: Sender<SignalData>,
    /// Feed outbound control (perm/bye/input) into the transport thread.
    ctl_tx: Sender<Vec<u8>>,
    threads: Vec<JoinHandle<()>>,
}

/// Host role: build the offer, create channels, send the offer over signaling,
/// and start the capture→encode→transport pipeline.
pub fn begin_host(engine: &Engine, peer: String, permission: Permission) {
    teardown(engine); // ensure clean slate
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();

    // Build the offerer Rtc and its data channels via the SDP API, then relay
    // the offer through the Cloudflare signaling (opaque `signal.data`, §6).
    let mut rtc = str0m::Rtc::new(std::time::Instant::now());
    let mut api = rtc.sdp_api();
    api.add_channel("video".to_string());
    api.add_channel("ctl".to_string());
    api.add_channel("cursor".to_string());
    let pending = match api.apply() {
        Some((offer, pending)) => {
            let _ = engine.signaling().send(protocol::SignalMsg::Signal {
                to: Some(peer.clone()),
                from: None,
                data: SignalData::Offer {
                    sdp: offer.to_sdp_string(),
                },
            });
            Some(pending)
        }
        None => None,
    };

    // Transport driver thread (owns the Rtc + UDP socket).
    let stop_t = stop.clone();
    let video_none: Option<Sender<Vec<u8>>> = None; // host does not render video
    let t = std::thread::spawn(move || {
        transport_driver(
            rtc,
            pending,
            signal_rx,
            ctl_rx,
            Some(frame_rx),
            video_none,
            stop_t,
        );
    });

    // Host capture→encode thread (own D3D11 device shared capture↔encode).
    let stop_m = stop.clone();
    let m = std::thread::spawn(move || {
        if let Err(e) = host_media_loop(frame_tx, stop_m) {
            tracing::warn!("host media loop ended: {e}");
        }
    });

    // Send the initial permission once the ctl channel is up (§4.2).
    let _ = ctl_tx.send(serialize(&ControlMsg::Perm { value: permission }));

    *SESSION.lock() = Some(Session {
        stop,
        signal_tx,
        ctl_tx,
        threads: vec![t, m],
    });
    let _ = peer;
}

/// Viewer role: accept the host's offer when it arrives (see [`on_signal`]) and
/// start the transport→decode→render pipeline. Called after `connect-response`
/// accepted; the actual offer is handled in [`on_signal`].
pub fn begin_viewer(_engine: &Engine, peer: String, _permission: Permission) {
    tracing::info!("viewer session with {peer}; awaiting offer");
    // The viewer's transport/media threads are started on the first offer in
    // `on_signal` (it needs the offer to build the answerer Rtc).
}

/// Route an inbound WebRTC payload (offer/answer/ICE) to the transport thread,
/// or bootstrap the viewer's answerer Rtc on the first offer.
pub fn on_signal(engine: &Engine, peer: &str, data: SignalData) {
    // If we already have a session, forward answer/ICE to its transport thread.
    if let Some(s) = SESSION.lock().as_ref() {
        let _ = s.signal_tx.send(data);
        return;
    }
    // No session yet + an offer ⇒ we are the viewer; build the answerer.
    if let SignalData::Offer { sdp } = data {
        start_viewer_transport(engine, peer.to_string(), sdp);
    }
}

fn start_viewer_transport(engine: &Engine, peer: String, offer_sdp: String) {
    let stop = Arc::new(AtomicBool::new(false));
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<SignalData>();
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (video_tx, video_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let mut rtc = str0m::Rtc::new(std::time::Instant::now());
    if let Ok(offer) = str0m::change::SdpOffer::from_sdp_string(&offer_sdp) {
        match rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => {
                let _ = engine.signaling().send(protocol::SignalMsg::Signal {
                    to: Some(peer.clone()),
                    from: None,
                    data: SignalData::Answer {
                        sdp: answer.to_sdp_string(),
                    },
                });
            }
            Err(e) => tracing::warn!("accept_offer failed: {e}"),
        }
    }

    // Transport thread (viewer: routes reassembled video to the render thread).
    let stop_t = stop.clone();
    let frame_none: Option<Receiver<(Vec<u8>, bool)>> = None;
    let t = std::thread::spawn(move || {
        transport_driver(
            rtc,
            None,
            signal_rx,
            ctl_rx,
            frame_none,
            Some(video_tx),
            stop_t,
        );
    });

    // Viewer decode→render thread.
    let stop_r = stop.clone();
    let r = std::thread::spawn(move || {
        if let Err(e) = viewer_media_loop(video_rx, stop_r) {
            tracing::warn!("viewer media loop ended: {e}");
        }
    });

    *SESSION.lock() = Some(Session {
        stop,
        signal_tx,
        ctl_tx,
        threads: vec![t, r],
    });
}

/// Send a host→viewer control message (perm change / bye) on the ctl channel.
pub fn send_ctl(_engine: &Engine, msg: &ControlMsg) {
    if let Some(s) = SESSION.lock().as_ref() {
        let _ = s.ctl_tx.send(serialize(msg));
    }
}

/// Tear the session down: signal all threads to stop and join them.
pub fn teardown(_engine: &Engine) {
    let session = SESSION.lock().take();
    if let Some(session) = session {
        session.stop.store(true, Ordering::SeqCst);
        for t in session.threads {
            let _ = t.join();
        }
    }
}

fn serialize(msg: &ControlMsg) -> Vec<u8> {
    serde_json::to_vec(msg).unwrap_or_default()
}

// ---- Transport driver (owns the str0m Rtc + UDP socket) ---------------------

#[allow(clippy::too_many_arguments)]
fn transport_driver(
    rtc: str0m::Rtc,
    mut pending: Option<str0m::change::SdpPendingOffer>,
    signal_rx: Receiver<SignalData>,
    ctl_rx: Receiver<Vec<u8>>,
    frame_rx: Option<Receiver<(Vec<u8>, bool)>>,
    video_tx: Option<Sender<Vec<u8>>>,
    stop: Arc<AtomicBool>,
) {
    use std::net::UdpSocket;
    use transport::{Inbound, Transport};

    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("udp bind failed: {e}");
            return;
        }
    };
    let _ = socket.set_nonblocking(true);

    let mut tp = Transport::new(rtc);
    // Advertise our host candidate (LAN direct path; STUN/TURN is the §6 fallback).
    if let Ok(local) = socket.local_addr() {
        if let Ok(cand) = str0m::Candidate::host(local, "udp") {
            let _ = tp.rtc_mut().add_local_candidate(cand);
        }
    }
    // Host creates the three channels here (direct write side).
    if frame_rx.is_some() {
        tp.create_channels();
    }

    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::SeqCst) {
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
        while let Ok(bytes) = ctl_rx.try_recv() {
            let _ = tp.send_ctl(&bytes);
        }
        if let Some(frame_rx) = &frame_rx {
            while let Ok((au, keyframe)) = frame_rx.try_recv() {
                let _ = tp.send_video(&au, keyframe);
            }
        }

        // 3) Drive str0m: emit transmits, handle timeouts, surface events.
        match tp.poll_output() {
            Ok(str0m::Output::Transmit(t)) => {
                let _ = socket.send_to(&t.contents, t.destination);
            }
            Ok(str0m::Output::Timeout(_)) => {
                // 4) Pull any pending UDP and feed it in; else advance time.
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if let Ok(local) = socket.local_addr() {
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
                    Err(_) => {
                        let _ = tp.handle_input(str0m::Input::Timeout(std::time::Instant::now()));
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
            Ok(str0m::Output::Event(ev)) => {
                if let Some(inbound) = tp.on_event(ev) {
                    match inbound {
                        Inbound::Video(frame) => {
                            if let Some(tx) = &video_tx {
                                let _ = tx.send(frame.data);
                            }
                        }
                        Inbound::Ctl(_bytes) => { /* host: inject; viewer: perm/bye */ }
                        Inbound::Disconnected => break,
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("transport poll error: {e}");
                break;
            }
        }
    }
}

// ---- Host capture → encode --------------------------------------------------

fn host_media_loop(
    frame_tx: Sender<(Vec<u8>, bool)>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use codec::{Encoder, EncoderConfig};

    let mut dup = capture::Duplicator::new(0, 0)?;
    // Encoder shares the capture device (§5c zero-copy).
    let mut encoder = Encoder::new(dup.device(), EncoderConfig::default())?;

    while !stop.load(Ordering::SeqCst) {
        match dup.acquire(std::time::Duration::from_millis(16)) {
            Ok(Some(frame)) => {
                if frame.pointer_only {
                    dup.release();
                    continue;
                }
                // NV12 conversion + encode happen inside the encoder path (§5b).
                match encoder.encode(&frame.texture) {
                    Ok(units) => {
                        for u in units {
                            let _ = frame_tx.send((u.data, u.keyframe));
                        }
                    }
                    Err(e) => tracing::debug!("encode: {e}"),
                }
                dup.release();
            }
            Ok(None) => { /* §5a idle: send nothing */ }
            Err(capture::Error::AccessLost) => {
                let _ = dup.reinit();
            }
            Err(e) => {
                tracing::warn!("capture: {e}");
                break;
            }
        }
    }
    Ok(())
}

// ---- Viewer decode → render -------------------------------------------------

fn viewer_media_loop(
    video_rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use codec::{Codec, Decoder};

    let hwnd_raw = RENDER_HWND.load(Ordering::SeqCst);
    if hwnd_raw == 0 {
        return Err("no render target set".into());
    }
    let hwnd = windows::Win32::Foundation::HWND(hwnd_raw as *mut _);

    // Create a device + decoder + renderer that share it (§5c/§7).
    let renderer_dev = create_render_device()?;
    let mut decoder = Decoder::new(&renderer_dev, Codec::H264, 1920, 1080)?;
    let mut renderer = render::Renderer::new(&renderer_dev, hwnd, 1920, 1080)?;

    let mut ts = 0i64;
    while !stop.load(Ordering::SeqCst) {
        match video_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(au) => {
                ts += 1;
                match decoder.decode(&au, ts) {
                    Ok(Some(frame)) => {
                        let _ = renderer.render_frame(&frame.texture, frame.array_index);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("decode: {e}"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Create a standalone D3D11 device for the viewer decode+render side.
fn create_render_device(
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Device, Box<dyn std::error::Error>> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_SDK_VERSION,
    };
    let mut device = None;
    // SAFETY: standard device creation for video decode + render.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.ok_or_else(|| "device creation returned null".into())
}
