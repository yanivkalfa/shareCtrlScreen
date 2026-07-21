//! Viewer → Host input events (contract §4.1). One compact JSON object per
//! data-channel message; `t` is the type discriminant. The host ignores all of
//! these unless the current permission is `control`.
//!
//! `mm` rides the unreliable channel; the rest ride the reliable channel (a lost
//! key-up or mouse-up would leave stuck input — see §6 channel split).

use serde::{Deserialize, Serialize};

/// Mouse button id, matching DOM `MouseEvent.button` (contract §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
}

impl TryFrom<u8> for Button {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Button::Left),
            1 => Ok(Button::Middle),
            2 => Ok(Button::Right),
            _ => Err("invalid mouse button"),
        }
    }
}

impl From<Button> for u8 {
    fn from(b: Button) -> u8 {
        b as u8
    }
}

/// A single input event (contract §4.1). Coordinates are normalized floats in
/// `[0,1]` relative to the full captured screen, top-left origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum InputMsg {
    /// Mouse move (unreliable channel). Throttled to ≤60 msg/s by the sender.
    #[serde(rename = "mm")]
    Move { x: f64, y: f64 },
    /// Mouse button down at a position (includes coords so the click lands even
    /// if a preceding `mm` was dropped).
    #[serde(rename = "md")]
    ButtonDown { b: Button, x: f64, y: f64 },
    /// Mouse button up.
    #[serde(rename = "mu")]
    ButtonUp { b: Button, x: f64, y: f64 },
    /// Wheel. `dx`/`dy` already in Windows wheel units (multiples of ±120).
    #[serde(rename = "wh")]
    Wheel { dx: i32, dy: i32 },
    /// Key down. `code` is the DOM `KeyboardEvent.code` (physical, layout-indep).
    #[serde(rename = "kd")]
    KeyDown { code: String },
    /// Key up.
    #[serde(rename = "ku")]
    KeyUp { code: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_wire_shape() {
        let m = InputMsg::Move {
            x: 0.5321,
            y: 0.201,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["t"], "mm");
        assert_eq!(v["x"], 0.5321);
    }

    #[test]
    fn button_roundtrip() {
        let m = InputMsg::ButtonDown {
            b: Button::Right,
            x: 0.5,
            y: 0.2,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"t\":\"md\""));
        assert!(s.contains("\"b\":2"));
        let back: InputMsg = serde_json::from_str(&s).unwrap();
        matches!(
            back,
            InputMsg::ButtonDown {
                b: Button::Right,
                ..
            }
        );
    }
}
