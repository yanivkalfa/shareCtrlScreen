//! Host → Viewer session-control messages (contract §4.2), always on the
//! reliable channel. Also the viewer's graceful `bye`.

use super::config::Permission;
use serde::{Deserialize, Serialize};

/// A control message (contract §4.2). `t` is the type discriminant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ControlMsg {
    /// Host changed the live permission. Sent once when the channel opens
    /// (initial value) and again on every change.
    #[serde(rename = "perm")]
    Perm { value: Permission },
    /// Host is ending the session (or viewer disconnecting gracefully).
    #[serde(rename = "bye")]
    Bye,
    /// Viewer → Host: request a fresh keyframe (decoder loss / just joined).
    /// Not in the original browser contract; the native engine uses it to drive
    /// the encoder's forced-IDR/LTR recovery (Plan 04 §5b).
    #[serde(rename = "kf")]
    KeyframeRequest,
    /// Host → Viewer: out-of-band cursor position + optional shape id
    /// (Plan 04 §5a/§7 — cursor rendered client-side so it feels instant).
    /// `x`/`y` normalized `[0,1]`. `shape` references a shape sent separately.
    #[serde(rename = "cur")]
    Cursor {
        x: f64,
        y: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        shape: Option<u32>,
        #[serde(default = "default_visible", skip_serializing_if = "is_true")]
        visible: bool,
    },
    /// Viewer → Host: the pixel size the viewer is actually displaying video at.
    /// The host encodes to exactly this (never upscaling past its own screen), so
    /// the viewer can present 1:1. Otherwise the frame is resampled on display —
    /// at a non-integer ratio that visibly softens every glyph — and bits are
    /// spent on pixels the viewer never shows.
    #[serde(rename = "vsize")]
    ViewSize { width: u32, height: u32 },
    /// Liveness probe, sent by BOTH sides on the reliable channel. The peer
    /// echoes it back as [`ControlMsg::Pong`]. Silence for the grace period means
    /// the session is dead even when the socket never reported an error — the
    /// case where a controller sat on a frozen picture believing it was live.
    #[serde(rename = "ping")]
    Ping { seq: u32 },
    /// Reply to [`ControlMsg::Ping`], echoing its `seq`.
    #[serde(rename = "pong")]
    Pong { seq: u32 },
    /// Clipboard text, synced in BOTH directions during a session. Text only —
    /// never file lists or rendered formats. Size-capped by the sender.
    #[serde(rename = "clip")]
    Clipboard { text: String },
}

fn default_visible() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_wire_shape() {
        let m = ControlMsg::Perm {
            value: Permission::Control,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"t\":\"perm\""));
        assert!(s.contains("\"value\":\"control\""));
    }

    #[test]
    fn bye_wire_shape() {
        let s = serde_json::to_string(&ControlMsg::Bye).unwrap();
        assert_eq!(s, "{\"t\":\"bye\"}");
    }
}
