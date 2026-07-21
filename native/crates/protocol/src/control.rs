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
