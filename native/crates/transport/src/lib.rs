//! Transport over `str0m` — sans-IO WebRTC, **no jitter buffer of our own, no
//! threads/timers** (Plan 04 §6). str0m gives standards-compliant ICE (NAT
//! traversal) + DTLS (encryption); we own all I/O so latency goes to the floor.
//! It reuses our Cloudflare WebSocket signaling unchanged (str0m's offer/answer +
//! trickle ICE relayed as the opaque `signal.data` payload).
//!
//! **Video rides an RTP media track, not a data channel.** This is the whole
//! point of the rewrite: a media track gets str0m's real-time video stack —
//! Google Congestion Control bandwidth estimation (BWE/TWCC), RTP packetization,
//! automatic NACK loss-repair from the send buffer, pacing, and in-order frame
//! reassembly with a bounded reorder window. That is exactly what the browser
//! build used to feel smooth; a data channel only offers general-purpose SCTP
//! flow control, which is why the data-channel video stalled and never matched
//! AnyDesk. Control and cursor stay on data channels where reliability/ordering
//! semantics matter more than a specialized congestion controller.
//!
//! | carrier            | reliability            | carries                      |
//! |--------------------|------------------------|------------------------------|
//! | `video` RTP media  | unreliable + NACK + BWE| encoded H.264 frames         |
//! | `ctl` data channel | reliable, ordered      | input, permission, cursor sh.|
//! | `cursor` data ch.  | unreliable-latest      | high-rate cursor position    |

pub mod fec;
pub mod packet;

use str0m::bwe::BweKind;
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::media::{Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt};
use str0m::{Event, Input, Output, Rtc, RtcError};

pub use std::time::Instant;
pub use str0m::net::{Protocol, Receive, Transmit};
pub use str0m::{Candidate, IceConnectionState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("str0m: {0}")]
    Rtc(#[from] RtcError),
    #[error("channel {0} not open")]
    ChannelClosed(&'static str),
}

/// The two data-channel ids once opened (§6). Video is an RTP media track (see
/// [`Transport::set_video_mid`]), negotiated via the engine's SDP API, so it is
/// not a channel here.
#[derive(Debug, Clone, Copy)]
pub struct Channels {
    pub ctl: ChannelId,
    pub cursor: ChannelId,
}

/// The two §6 data-channel configs. The host creates both when building the
/// offer; the viewer matches them by label. Video is added separately as a media
/// track in the SDP offer (see `begin_host` in the engine).
pub fn channel_configs() -> [ChannelConfig; 2] {
    [
        ChannelConfig {
            label: "ctl".to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        },
        ChannelConfig {
            label: "cursor".to_string(),
            ordered: false,
            // Unreliable-latest: a stale cursor position is worthless.
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        },
    ]
}

/// Inbound payloads surfaced to the engine after routing a str0m event.
#[derive(Debug, Clone)]
pub enum Inbound {
    Connected,
    Disconnected,
    /// A whole decoded video frame from the RTP media track (viewer side). str0m
    /// has already depacketized and reassembled it; `contiguous` is false when a
    /// gap (unrecovered loss) preceded it, which may warrant a keyframe request.
    Video {
        data: Vec<u8>,
        keyframe: bool,
        contiguous: bool,
    },
    /// The remote peer asked us (the host) for a keyframe (PLI/FIR over RTCP).
    KeyframeRequest,
    /// A fresh send-bandwidth estimate in bits/sec from str0m's congestion
    /// control (host side). This is the real BWE that steers the encoder bitrate,
    /// replacing the old hand-rolled AIMD on the SCTP send-queue depth.
    BweEstimate(u32),
    /// A reliable control message (host: input; viewer: perm/bye/cursor-shape).
    Ctl(Vec<u8>),
    /// A cursor position update.
    Cursor(Vec<u8>),
    /// A data channel opened; once both are known the engine has [`Channels`].
    ChannelOpen(ChannelId, String),
}

/// str0m wrapper holding the data channels and the video media track.
pub struct Transport {
    rtc: Rtc,
    ctl: Option<ChannelId>,
    cursor: Option<ChannelId>,
    /// The video RTP media track's mid. Host: set from `add_media` via
    /// [`Self::set_video_mid`]. Viewer: learned from `Event::MediaAdded`.
    video_mid: Option<Mid>,
    /// Negotiated video payload type + clock, resolved lazily on first send once
    /// the SDP answer is in (the remote PTs aren't known before that).
    video_pt: Option<Pt>,
    video_freq: Option<Frequency>,
    /// Monotonic origin for RTP media timestamps.
    media_start: Instant,
}

impl Transport {
    /// Wrap an already-built [`Rtc`]. The host builds it as offerer (adding the
    /// video media track + data channels), the viewer as answerer.
    pub fn new(rtc: Rtc) -> Self {
        Self {
            rtc,
            ctl: None,
            cursor: None,
            video_mid: None,
            video_pt: None,
            video_freq: None,
            media_start: Instant::now(),
        }
    }

    /// Host side: adopt the two §6 data-channel ids created on the `Rtc` before it
    /// was handed to this transport.
    pub fn set_channels(&mut self, ch: Channels) {
        self.ctl = Some(ch.ctl);
        self.cursor = Some(ch.cursor);
    }

    /// Host side: adopt the video media track's mid returned by `add_media`.
    pub fn set_video_mid(&mut self, mid: Mid) {
        self.video_mid = Some(mid);
    }

    pub fn rtc_mut(&mut self) -> &mut Rtc {
        &mut self.rtc
    }

    /// Drive the sans-IO state machine one step (§6: we own the loop).
    pub fn poll_output(&mut self) -> Result<Output, Error> {
        Ok(self.rtc.poll_output()?)
    }

    /// Feed a timeout or a received datagram back into str0m.
    pub fn handle_input(&mut self, input: Input<'_>) -> Result<(), Error> {
        self.rtc.handle_input(input)?;
        Ok(())
    }

    /// Host: tell the BWE the bitrate we'd like to eventually reach, so it probes
    /// up toward it. The actual estimate comes back via
    /// `Event::EgressBitrateEstimate` → [`Inbound::BweEstimate`].
    pub fn set_desired_bitrate(&mut self, bps: u32) {
        self.rtc
            .bwe()
            .set_desired_bitrate(str0m::bwe::Bitrate::bps(bps as u64));
    }

    /// Host: write one encoded H.264 access unit (Annex-B) onto the video media
    /// track. str0m packetizes it into RTP (FU-A as needed), paces it, and repairs
    /// loss via NACK from its send buffer. A no-op until the track is negotiated
    /// and connected — str0m drops pre-connect media anyway, and the engine forces
    /// a keyframe on connect so the first *delivered* frame is decodable.
    pub fn send_video(&mut self, au: &[u8]) -> Result<(), Error> {
        let Some(mid) = self.video_mid else {
            return Ok(());
        };
        // Resolve the negotiated PT + clock once. `payload_params()` is filtered by
        // the remote's PTs, which are only populated after the answer is accepted;
        // before that it's empty and we skip (the frame would be dropped anyway).
        if self.video_pt.is_none() {
            let resolved = self.rtc.writer(mid).and_then(|w| {
                w.payload_params()
                    .next()
                    .map(|p| (p.pt(), p.spec().clock_rate))
            });
            if let Some((pt, freq)) = resolved {
                self.video_pt = Some(pt);
                self.video_freq = Some(freq);
            }
        }
        let (Some(pt), Some(freq)) = (self.video_pt, self.video_freq) else {
            return Ok(());
        };

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.media_start);
        // RTP timestamp in the codec clock (90 kHz for video).
        let ticks = (elapsed.as_micros() as u64) * freq.get() as u64 / 1_000_000;
        let rtp_time = MediaTime::new(ticks, freq);

        if let Some(writer) = self.rtc.writer(mid) {
            writer.write(pt, now, rtp_time, au)?;
        }
        Ok(())
    }

    /// Viewer: ask the sender for a fresh keyframe via native RTCP PLI. (The
    /// engine also has a reliable `ctl`-channel keyframe request; this is the
    /// RTP-native path, used when str0m's reorder window reports a gap.)
    pub fn request_keyframe(&mut self) {
        if let Some(mid) = self.video_mid {
            if let Some(mut writer) = self.rtc.writer(mid) {
                let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
            }
        }
    }

    /// Send a reliable control payload (input event / perm / bye).
    pub fn send_ctl(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let id = self.ctl.ok_or(Error::ChannelClosed("ctl"))?;
        if let Some(mut ch) = self.rtc.channel(id) {
            ch.write(true, bytes)?;
        }
        Ok(())
    }

    /// Send a cursor position update on the unreliable-latest cursor channel.
    pub fn send_cursor(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let id = self.cursor.ok_or(Error::ChannelClosed("cursor"))?;
        if let Some(mut ch) = self.rtc.channel(id) {
            ch.write(true, bytes)?;
        }
        Ok(())
    }

    /// Route a str0m [`Event`] into an [`Inbound`]. Returns `None` for events the
    /// engine does not need to act on.
    pub fn on_event(&mut self, event: Event) -> Option<Inbound> {
        match event {
            Event::Connected => {
                tracing::info!("transport: DTLS/ICE connected");
                Some(Inbound::Connected)
            }
            Event::IceConnectionStateChange(state) => {
                tracing::info!("transport: ICE state = {state:?}");
                if state == IceConnectionState::Disconnected {
                    Some(Inbound::Disconnected)
                } else {
                    None
                }
            }
            Event::MediaAdded(m) => {
                // Viewer: learn the incoming video track's mid (host set it from
                // add_media). Nothing for the engine to act on directly.
                if m.kind == MediaKind::Video {
                    self.video_mid = Some(m.mid);
                    tracing::info!("transport: video media track added ({:?})", m.mid);
                }
                None
            }
            Event::MediaData(d) => {
                if d.data.is_empty() {
                    return None;
                }
                Some(Inbound::Video {
                    keyframe: d.is_keyframe(),
                    contiguous: d.contiguous,
                    data: d.data.to_vec(),
                })
            }
            Event::KeyframeRequest(_) => Some(Inbound::KeyframeRequest),
            Event::EgressBitrateEstimate(kind) => {
                let bps = match kind {
                    BweKind::Twcc(b) | BweKind::Remb(_, b) => b.as_u64(),
                    _ => return None,
                };
                Some(Inbound::BweEstimate(bps.min(u32::MAX as u64) as u32))
            }
            Event::ChannelOpen(id, label) => {
                // Learn ids on the viewer side (channels created by the host).
                match label.as_str() {
                    "ctl" => self.ctl = Some(id),
                    "cursor" => self.cursor = Some(id),
                    _ => {}
                }
                Some(Inbound::ChannelOpen(id, label))
            }
            Event::ChannelData(data) => {
                if Some(data.id) == self.ctl {
                    Some(Inbound::Ctl(data.data))
                } else if Some(data.id) == self.cursor {
                    Some(Inbound::Cursor(data.data))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
