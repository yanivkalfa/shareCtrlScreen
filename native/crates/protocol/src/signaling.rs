//! WebSocket signaling messages (contract §3) — the wire contract with the
//! Cloudflare Worker, reused **verbatim**. The server relays these as text
//! frames, one JSON object per frame, replacing `to` with `from`.
//!
//! Only the opaque `data` payload of a `signal` message changes vs. the Electron
//! app: it now carries str0m's offer/answer/ICE instead of browser SDP. The
//! server treats `data` as opaque, so this is transparent to it.

use serde::{Deserialize, Serialize};

/// A message on the signaling socket. Tagged by the `type` field (contract §3).
///
/// `to` is set by a sender that wants the message relayed to a peer; the server
/// strips it and injects `from` (the sender's registered UUID) before forwarding.
/// Both are optional here so one type serves inbound and outbound.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SignalMsg {
    /// First message after connecting (contract §3.1).
    Register {
        uuid: String,
        v: u32,
    },
    Registered {
        uuid: String,
    },
    RegisterError {
        reason: String, // "duplicate" | "invalid-uuid" | "bad-version"
    },

    /// Viewer → Host (contract §3.2 step 1). `password` is null on first attempt
    /// or the challenge-response **proof** string afterwards.
    ConnectRequest {
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default)]
        password: Option<String>,
        /// Optional codec capability advertisement (contract §3.2 v1.1).
        #[serde(skip_serializing_if = "Option::is_none")]
        caps: Option<Caps>,
    },

    /// Host → Viewer when in password mode with a null password (contract §3.2).
    PasswordRequired {
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// 16 random bytes as lower-case hex, fresh per attempt.
        nonce: String,
    },

    /// Host → Viewer decision (contract §3.2 step 3).
    ConnectResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        accepted: bool,
        /// Present only when `accepted` — "view" | "control".
        #[serde(skip_serializing_if = "Option::is_none")]
        permission: Option<String>,
        /// Present only when rejected — "denied"|"busy"|"bad-password"|"timeout".
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Negotiated codec the host will stream (§3), e.g. "H264"/"HEVC"/"AV1".
        /// The viewer decodes with this. Absent ⇒ H.264 default. The server
        /// relays it opaquely (no server change).
        #[serde(skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
    },

    /// WebRTC negotiation (contract §3.2 step 4). `data` is opaque to the server.
    Signal {
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        data: SignalData,
    },

    /// Session end, best-effort (contract §3.2 step 5).
    EndSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },

    /// Server → sender when a `to` target is offline (contract §3.3).
    RelayError {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },

    Ping,
    Pong,
}

/// Codec capability advertisement (contract §3.2 v1.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caps {
    pub decode: Vec<String>,
}

/// The opaque WebRTC negotiation payload carried inside `signal.data`
/// (contract §3.2 step 4). With str0m these carry its handshake rather than
/// browser SDP, but keep the same discriminant shape (`kind`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SignalData {
    Offer { sdp: String },
    Answer { sdp: String },
    Ice { candidate: IceCandidate },
}

/// Serialized ICE candidate (contract §3.2: `candidate`, `sdpMid`,
/// `sdpMLineIndex`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMid", skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex", skip_serializing_if = "Option::is_none")]
    pub sdp_mline_index: Option<u32>,
}

/// The exact literal ping/pong strings (contract §3.3). These MUST be byte-exact
/// — the server matches the ping as a raw string, not parsed JSON.
pub const PING_LITERAL: &str = "{\"type\":\"ping\"}";
pub const PONG_LITERAL: &str = "{\"type\":\"pong\"}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_literal_matches_contract() {
        // Serializing the enum must produce the exact literal the server expects.
        let s = serde_json::to_string(&SignalMsg::Ping).unwrap();
        assert_eq!(s, PING_LITERAL);
        let s = serde_json::to_string(&SignalMsg::Pong).unwrap();
        assert_eq!(s, PONG_LITERAL);
    }

    #[test]
    fn connect_request_omits_null_routing_fields() {
        let m = SignalMsg::ConnectRequest {
            to: Some("host-uuid".into()),
            from: None,
            password: None,
            caps: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["type"], "connect-request");
        assert_eq!(v["to"], "host-uuid");
        assert!(v.get("from").is_none());
        // password:null must still be present per contract (explicit null first try).
        assert!(v["password"].is_null());
    }

    #[test]
    fn signal_data_roundtrip() {
        let m = SignalMsg::Signal {
            to: Some("peer".into()),
            from: None,
            data: SignalData::Ice {
                candidate: IceCandidate {
                    candidate: "candidate:1 1 udp ...".into(),
                    sdp_mid: Some("0".into()),
                    sdp_mline_index: Some(0),
                },
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: SignalMsg = serde_json::from_str(&s).unwrap();
        matches!(back, SignalMsg::Signal { .. });
    }
}
