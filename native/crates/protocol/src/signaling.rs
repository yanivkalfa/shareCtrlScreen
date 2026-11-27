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
        /// The host's operating-system family: "windows" | "macos" | "linux".
        /// The viewer translates modifier keys for it (Cmd ↔ Ctrl, Cmd+Tab →
        /// Alt+Tab). Additive: absent (older hosts) or an unknown value means
        /// "unknown", and the viewer then forwards keys literally. Relayed
        /// opaquely by the server, like `codec`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        os: Option<String>,
        /// What the host supports beyond the base contract (plans/08 P6). Absent
        /// (older hosts) means nothing; older viewers ignore it. Relayed
        /// opaquely by the server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caps: Option<HostCaps>,
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

    /// Client → server: mint TURN relay credentials for this session. The server
    /// generates them from its Cloudflare TURN key (no per-user config).
    Turn,
    /// Server → client: the minted relay credentials (or an `error`). Sent over
    /// the same socket so no extra connection/TLS is needed.
    TurnCredentials {
        #[serde(rename = "iceServers", skip_serializing_if = "Option::is_none")]
        ice_servers: Option<IceServers>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    Ping,
    Pong,
}

/// TURN/STUN credentials as returned by Cloudflare's credential endpoint: a set
/// of `urls` sharing one `username`/`credential`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServers {
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Codec capability advertisement (contract §3.2 v1.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caps {
    pub decode: Vec<String>,
    /// The viewer can show more host monitors in pop-out windows (D14), so the
    /// host may offer spare video tracks. Older viewers omit it and get the
    /// single-track session they understand; older hosts ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub popout: bool,
    /// The viewer understands [`crate::ControlMsg::HostState`] (plans/10
    /// U1): the host tells it when its screen is a lock screen or another
    /// secure screen. A host sends `hstate` only to a viewer that set this;
    /// older viewers omit it and never receive one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hstate: bool,
}

/// The host's capabilities, in its connect-response (the counterpart of the
/// viewer's [`Caps`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCaps {
    /// The host pauses the main video on [`crate::ControlMsg::VideoPause`] and
    /// resumes it on [`crate::ControlMsg::VideoResume`] (background tabs,
    /// plans/08 §3.7). Without it the host keeps streaming to a background tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vpause: bool,
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

    /// D14: pop-out support rides on `caps` without disturbing either side's
    /// older peers: an old viewer's caps read as "no pop-outs", and a viewer
    /// that cannot show them sends exactly the old shape.
    #[test]
    fn popout_capability_is_optional_both_ways() {
        let old: Caps = serde_json::from_str(r#"{"decode":["H264"]}"#).unwrap();
        assert!(!old.popout);
        let none = Caps {
            decode: vec!["H264".into()],
            popout: false,
            hstate: false,
        };
        assert_eq!(
            serde_json::to_string(&none).unwrap(),
            r#"{"decode":["H264"]}"#
        );
        let new = Caps {
            decode: vec![],
            popout: true,
            hstate: false,
        };
        assert!(serde_json::to_string(&new)
            .unwrap()
            .contains(r#""popout":true"#));
    }

    /// plans/10 U1: `hstate` support rides on `caps` like `popout`: an old
    /// viewer's caps read as "no hstate" (so it is never sent one), and the
    /// flag is on the wire only when set.
    #[test]
    fn hstate_capability_is_optional_both_ways() {
        let old: Caps = serde_json::from_str(r#"{"decode":["H264"],"popout":true}"#).unwrap();
        assert!(!old.hstate);
        let new = Caps {
            decode: vec!["H264".into()],
            popout: true,
            hstate: true,
        };
        let s = serde_json::to_string(&new).unwrap();
        assert_eq!(s, r#"{"decode":["H264"],"popout":true,"hstate":true}"#);
        assert!(serde_json::from_str::<Caps>(&s).unwrap().hstate);
        let off = Caps {
            hstate: false,
            ..new
        };
        assert!(!serde_json::to_string(&off).unwrap().contains("hstate"));
    }

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
    fn connect_response_os_is_optional_and_additive() {
        let m = SignalMsg::ConnectResponse {
            to: Some("viewer".into()),
            from: None,
            accepted: true,
            permission: Some("control".into()),
            reason: None,
            codec: Some("H264".into()),
            os: Some("macos".into()),
            caps: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["os"], "macos");
        // A response from a host that predates the field still parses (as
        // "unknown"), and a response without it carries no `os` key.
        let old: SignalMsg = serde_json::from_str(
            r#"{"type":"connect-response","from":"h","accepted":true,"permission":"view"}"#,
        )
        .unwrap();
        assert!(matches!(old, SignalMsg::ConnectResponse { os: None, .. }));
        let none = SignalMsg::ConnectResponse {
            to: None,
            from: None,
            accepted: false,
            permission: None,
            reason: Some("busy".into()),
            codec: None,
            os: None,
            caps: None,
        };
        let text = serde_json::to_string(&none).unwrap();
        assert!(!text.contains("\"os\"") && !text.contains("\"caps\""));
    }

    #[test]
    fn host_caps_are_optional_and_additive() {
        let m = SignalMsg::ConnectResponse {
            to: None,
            from: None,
            accepted: true,
            permission: Some("view".into()),
            reason: None,
            codec: None,
            os: None,
            caps: Some(HostCaps { vpause: true }),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["caps"]["vpause"], true);
        // An older host sends none; an empty object reads as no capabilities.
        let old: SignalMsg = serde_json::from_str(
            r#"{"type":"connect-response","from":"h","accepted":true,"caps":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            SignalMsg::ConnectResponse {
                caps: Some(HostCaps { vpause: false }),
                ..
            }
        ));
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
