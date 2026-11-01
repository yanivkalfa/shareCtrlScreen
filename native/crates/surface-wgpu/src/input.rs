//! Input routing for a video surface: the sink, held-key/button tracking and
//! the synthesized releases the [`platform_api::VideoSurface`] invariants
//! demand, plus wheel accumulation.
//!
//! WHY held tracking exists: when the OS moves keyboard focus away mid-combo
//! (Cmd+Tab, a click in another window) the key-up lands in another window and
//! never reaches us, so the host would keep e.g. `Alt` down forever — the
//! classic remote-desktop stuck modifier. The Windows surface fixed it in
//! `render/src/window.rs`; this is the same logic, OS-neutral.

use std::sync::mpsc::Sender;

use platform_api::VideoInput;

/// The input sink plus everything currently held down on the surface.
#[derive(Debug, Default)]
pub struct InputState {
    sink: Option<Sender<VideoInput>>,
    held_keys: Vec<&'static str>,
    held_buttons: Vec<u8>,
    last_pos: Option<(f64, f64)>,
}

impl InputState {
    /// Install or clear the sink. Anything held is released through the
    /// **old** sink first: those presses went to that session's host, and the
    /// new sink (or none) must start with nothing held.
    pub fn set_sink(&mut self, sink: Option<Sender<VideoInput>>) {
        self.release_all();
        self.sink = sink;
    }

    pub fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    pub fn moved(&mut self, nx: f64, ny: f64) {
        self.last_pos = Some((nx, ny));
        self.emit(VideoInput::Move { nx, ny });
    }

    pub fn button(&mut self, button: u8, down: bool, nx: f64, ny: f64) {
        self.last_pos = Some((nx, ny));
        if down {
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else {
            self.held_buttons.retain(|&b| b != button);
        }
        self.emit(VideoInput::Button {
            button,
            down,
            nx,
            ny,
        });
    }

    pub fn wheel(&mut self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            self.emit(VideoInput::Wheel { dx, dy });
        }
    }

    /// Key by DOM code. Auto-repeat downs are forwarded (the host does not
    /// repeat injected keys on its own), and tracked once.
    pub fn key(&mut self, code: &'static str, down: bool) {
        if down {
            if !self.held_keys.contains(&code) {
                self.held_keys.push(code);
            }
        } else {
            self.held_keys.retain(|&k| k != code);
        }
        self.emit(VideoInput::Key { code, down });
    }

    pub fn is_key_held(&self, code: &str) -> bool {
        self.held_keys.contains(&code)
    }

    /// Synthesize key-up / button-up for everything held (focus loss, sink
    /// change). Keys go first so a held modifier never turns the button-up
    /// into a modified click on the host.
    pub fn release_all(&mut self) {
        let keys = std::mem::take(&mut self.held_keys);
        let buttons = std::mem::take(&mut self.held_buttons);
        // A release at the last place the pointer was seen; the centre is only
        // a fallback for a button that was somehow held without any position.
        let (nx, ny) = self.last_pos.unwrap_or((0.5, 0.5));
        for code in keys.into_iter().rev() {
            self.emit(VideoInput::Key { code, down: false });
        }
        for button in buttons {
            self.emit(VideoInput::Button {
                button,
                down: false,
                nx,
                ny,
            });
        }
    }

    fn emit(&self, ev: VideoInput) {
        if let Some(tx) = &self.sink {
            // A closed receiver means the session is ending; dropping the
            // event is correct, and the sink is cleared right after.
            let _ = tx.send(ev);
        }
    }
}

/// Turns fractional wheel deltas (trackpads report tiny pixel steps) into the
/// protocol's Windows units (one notch = ±120) without losing the remainder,
/// so slow scrolling still scrolls.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelAccumulator {
    x: f64,
    y: f64,
}

impl WheelAccumulator {
    /// Add a delta already scaled to Windows units; returns the whole units to
    /// send now (`(0, 0)` when still below one unit).
    pub fn add(&mut self, dx_units: f64, dy_units: f64) -> (i32, i32) {
        if dx_units.is_finite() {
            self.x += dx_units;
        }
        if dy_units.is_finite() {
            self.y += dy_units;
        }
        let out_x = self.x.trunc();
        let out_y = self.y.trunc();
        self.x -= out_x;
        self.y -= out_y;
        (clamp_i32(out_x), clamp_i32(out_y))
    }

    /// Drop any remainder (the gesture ended).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

fn clamp_i32(v: f64) -> i32 {
    v.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn drain(rx: &Receiver<VideoInput>) -> Vec<VideoInput> {
        rx.try_iter().collect()
    }

    #[test]
    fn clearing_the_sink_releases_held_input_first() {
        let (tx, rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(tx));
        s.key("ShiftLeft", true);
        s.key("KeyA", true);
        s.button(0, true, 0.25, 0.75);
        drain(&rx);
        s.set_sink(None);
        assert_eq!(
            drain(&rx),
            vec![
                VideoInput::Key {
                    code: "KeyA",
                    down: false
                },
                VideoInput::Key {
                    code: "ShiftLeft",
                    down: false
                },
                VideoInput::Button {
                    button: 0,
                    down: false,
                    nx: 0.25,
                    ny: 0.75
                },
            ]
        );
        assert!(!s.has_sink());
    }

    #[test]
    fn replacing_the_sink_releases_through_the_old_one() {
        let (old_tx, old_rx) = channel();
        let (new_tx, new_rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(old_tx));
        s.key("AltLeft", true);
        drain(&old_rx);
        s.set_sink(Some(new_tx));
        assert_eq!(
            drain(&old_rx),
            vec![VideoInput::Key {
                code: "AltLeft",
                down: false
            }]
        );
        assert!(drain(&new_rx).is_empty());
    }

    #[test]
    fn released_keys_are_not_released_again() {
        let (tx, rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(tx));
        s.key("KeyQ", true);
        s.key("KeyQ", true); // auto-repeat
        s.key("KeyQ", false);
        drain(&rx);
        s.release_all();
        assert!(drain(&rx).is_empty());
        assert!(!s.is_key_held("KeyQ"));
    }

    #[test]
    fn repeat_downs_are_forwarded_but_tracked_once() {
        let (tx, rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(tx));
        s.key("KeyJ", true);
        s.key("KeyJ", true);
        assert_eq!(drain(&rx).len(), 2);
        s.release_all();
        assert_eq!(drain(&rx).len(), 1);
    }

    #[test]
    fn button_release_uses_the_last_pointer_position() {
        let (tx, rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(tx));
        s.button(2, true, 0.1, 0.1);
        s.moved(0.9, 0.4);
        drain(&rx);
        s.release_all();
        assert_eq!(
            drain(&rx),
            vec![VideoInput::Button {
                button: 2,
                down: false,
                nx: 0.9,
                ny: 0.4
            }]
        );
    }

    #[test]
    fn without_a_sink_nothing_is_sent_but_state_is_kept_consistent() {
        let mut s = InputState::default();
        s.key("KeyA", true);
        assert!(s.is_key_held("KeyA"));
        s.release_all();
        assert!(!s.is_key_held("KeyA"));
    }

    #[test]
    fn zero_wheel_is_not_sent() {
        let (tx, rx) = channel();
        let mut s = InputState::default();
        s.set_sink(Some(tx));
        s.wheel(0, 0);
        s.wheel(0, -120);
        assert_eq!(drain(&rx), vec![VideoInput::Wheel { dx: 0, dy: -120 }]);
    }

    #[test]
    fn wheel_accumulator_keeps_the_fraction() {
        let mut w = WheelAccumulator::default();
        assert_eq!(w.add(0.4, 0.4), (0, 0));
        assert_eq!(w.add(0.4, 0.4), (0, 0));
        assert_eq!(w.add(0.4, 0.4), (1, 1));
        // x: 0.2 left over, + -2.5 = -2.3 → send -2, keep -0.3.
        assert_eq!(w.add(-2.5, 240.0), (-2, 240));
        // -0.3 + -0.8 = -1.1 → send -1.
        assert_eq!(w.add(-0.8, 0.0), (-1, 0));
    }

    #[test]
    fn wheel_accumulator_ignores_non_finite_and_resets() {
        let mut w = WheelAccumulator::default();
        assert_eq!(w.add(f64::NAN, f64::INFINITY), (0, 0));
        w.add(0.9, 0.9);
        w.reset();
        assert_eq!(w.add(0.2, 0.2), (0, 0));
    }
}
