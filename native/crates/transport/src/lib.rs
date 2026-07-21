//! Transport over `str0m` data channels — sans-IO WebRTC, **no jitter buffer,
//! no threads/timers of its own** (Plan 04 §6). str0m gives standards-compliant
//! ICE (NAT traversal) + DTLS (encryption) + SCTP framing; we own all I/O and
//! buffering so latency goes to the floor. It reuses our Cloudflare WebSocket
//! signaling unchanged (str0m's offer/answer + trickle ICE relayed as the opaque
//! `signal.data` payload).
//!
//! Three channels mirror §6:
//! | channel  | reliability                         | carries                        |
//! |----------|-------------------------------------|--------------------------------|
//! | `video`  | unreliable (`ordered=false`, 0 rtx) | encoded frames, FEC-protected  |
//! | `ctl`    | reliable, ordered                   | input, permission, cursor shape|
//! | `cursor` | unreliable-latest                   | high-rate cursor position      |
//!
//! Video AUs are app-fragmented ([`packet`]) and keyframes FEC-protected
//! ([`fec`]); delta frames are dropped rather than retransmitted (Parsec model).

pub mod fec;
pub mod packet;

use packet::{ReassembledFrame, Reassembler};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
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

/// The three channel ids once opened (§6).
#[derive(Debug, Clone, Copy)]
pub struct Channels {
    pub video: ChannelId,
    pub ctl: ChannelId,
    pub cursor: ChannelId,
}

/// §6 channel configs. The host creates all three when building the offer; the
/// viewer matches them by label.
pub fn channel_configs() -> [ChannelConfig; 3] {
    [
        ChannelConfig {
            label: "video".to_string(),
            ordered: false,
            // Unreliable: never retransmit — a lost delta frame is superseded.
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        },
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
    /// A fully reassembled video access unit (viewer side).
    Video(ReassembledFrame),
    /// A reliable control message (host: input; viewer: perm/bye/cursor-shape).
    Ctl(Vec<u8>),
    /// A cursor position update.
    Cursor(Vec<u8>),
    /// A data channel opened; once all three are known the engine has [`Channels`].
    ChannelOpen(ChannelId, String),
}

/// str0m wrapper holding the three channels and the video reassembler.
pub struct Transport {
    rtc: Rtc,
    video: Option<ChannelId>,
    ctl: Option<ChannelId>,
    cursor: Option<ChannelId>,
    reassembler: Reassembler,
    next_frame_id: u32,
}

impl Transport {
    /// Wrap an already-built [`Rtc`]. The host builds it as offerer and creates
    /// the channels via [`Self::create_channels`]; the viewer receives them via
    /// `Event::ChannelOpen`.
    pub fn new(rtc: Rtc) -> Self {
        Self {
            rtc,
            video: None,
            ctl: None,
            cursor: None,
            reassembler: Reassembler::new(),
            next_frame_id: 0,
        }
    }

    /// Host side: adopt the three §6 channel ids created on the `Rtc` before it
    /// was handed to this transport (they must be created exactly once — creating
    /// a second set under the same labels makes the viewer's label→id mapping
    /// ambiguous and doubles every ChannelOpen).
    pub fn set_channels(&mut self, ch: Channels) {
        self.video = Some(ch.video);
        self.ctl = Some(ch.ctl);
        self.cursor = Some(ch.cursor);
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

    /// Send an encoded access unit on the unreliable video channel: fragment to
    /// ≤MTU datagrams (§6). Keyframe framing is tagged so the viewer can request
    /// FEC/priority; FEC recovery shards for keyframes are added by the caller.
    pub fn send_video(&mut self, au: &[u8], keyframe: bool) -> Result<(), Error> {
        let id = self.video.ok_or(Error::ChannelClosed("video"))?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        for frag in packet::fragment(frame_id, keyframe, au) {
            if let Some(mut ch) = self.rtc.channel(id) {
                ch.write(true, &frag.0)?;
            }
        }
        Ok(())
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

    /// Route a str0m [`Event`] into an [`Inbound`], reassembling video fragments.
    /// Returns `None` for events the engine does not need to act on.
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
            Event::ChannelOpen(id, label) => {
                // Learn ids on the viewer side (channels created by the host).
                match label.as_str() {
                    "video" => self.video = Some(id),
                    "ctl" => self.ctl = Some(id),
                    "cursor" => self.cursor = Some(id),
                    _ => {}
                }
                Some(Inbound::ChannelOpen(id, label))
            }
            Event::ChannelData(data) => {
                if Some(data.id) == self.video {
                    self.reassembler.push(&data.data).map(Inbound::Video)
                } else if Some(data.id) == self.ctl {
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
