//! Hover reveal of the web session toolbar (the AnyDesk layout): the video
//! covers the whole window including the web `.view-bar`, so pushing the
//! pointer to the top edge uncovers the bar, and moving back down into the
//! picture covers it again. This is the surface's own job — no engine or UI
//! call drives it (see the [`platform_api::VideoSurface`] invariants).
//!
//! The thresholds are those of the Windows surface (`render/src/window.rs`):
//! reveal at the top 2 px, uncover 40 CSS px, hide again past 56 px. Here the
//! hide depth is in CSS px too (scaled like the bar). WHY: the pointer only
//! reaches the video below the bar, so with an unscaled 56 px and a 2× display
//! (bar = 80 px) every move under the bar would hide it at once — the 16 px of
//! hysteresis that lets the user move off the bar without it snapping shut
//! would vanish exactly on Retina screens.

/// Pointer within this many physical pixels of the top edge reveals the bar.
pub const REVEAL_EDGE_PX: f64 = 2.0;
/// Height of the web session bar in CSS px (= points on macOS).
pub const BAR_CSS_PX: f64 = 40.0;
/// Pointer deeper than this (CSS px from the surface top) hides the bar again.
pub const HIDE_DEPTH_CSS_PX: f64 = 56.0;

/// What the surface must do after a pointer update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealChange {
    /// Remove the top [`bar_px`] from paint and hit-testing.
    Reveal,
    /// Restore the full surface.
    Hide,
}

/// The reveal state machine. Pure: the backend feeds it pointer positions and
/// applies the returned change to its view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reveal {
    revealed: bool,
}

/// The bar height in physical pixels for a backing `scale` (2.0 on Retina).
pub fn bar_px(scale: f64) -> f64 {
    BAR_CSS_PX * sane_scale(scale)
}

fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

impl Reveal {
    pub fn is_revealed(&self) -> bool {
        self.revealed
    }

    /// Feed the pointer's distance from the top of the **full** surface in
    /// physical pixels (negative when it left through the top edge). Returns
    /// the change to apply, or `None` when nothing changes.
    pub fn on_pointer(&mut self, y_px: f64, scale: f64) -> Option<RevealChange> {
        let scale = sane_scale(scale);
        if !self.revealed && y_px <= REVEAL_EDGE_PX {
            self.revealed = true;
            Some(RevealChange::Reveal)
        } else if self.revealed && y_px > HIDE_DEPTH_CSS_PX * scale {
            self.revealed = false;
            Some(RevealChange::Hide)
        } else {
            None
        }
    }

    /// Forget any reveal (the surface was hidden or shown afresh). Returns
    /// `Some(Hide)` when the bar was up so the caller restores its frame.
    pub fn reset(&mut self) -> Option<RevealChange> {
        std::mem::take(&mut self.revealed).then_some(RevealChange::Hide)
    }

    /// How many physical pixels are currently removed from the top.
    pub fn inset_px(&self, scale: f64) -> f64 {
        if self.revealed {
            bar_px(scale)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveals_only_at_the_top_two_pixels() {
        let mut r = Reveal::default();
        assert_eq!(r.on_pointer(3.0, 1.0), None);
        assert_eq!(r.on_pointer(2.0, 1.0), Some(RevealChange::Reveal));
        assert!(r.is_revealed());
        // Repeated top-edge moves do not re-trigger.
        assert_eq!(r.on_pointer(0.0, 1.0), None);
    }

    #[test]
    fn leaving_through_the_top_edge_reveals() {
        let mut r = Reveal::default();
        assert_eq!(r.on_pointer(-1.0, 2.0), Some(RevealChange::Reveal));
    }

    #[test]
    fn hides_only_past_the_hysteresis_depth() {
        let mut r = Reveal::default();
        r.on_pointer(0.0, 1.0);
        // Just under the bar: still revealed (the user is moving off it).
        assert_eq!(r.on_pointer(41.0, 1.0), None);
        assert_eq!(r.on_pointer(56.0, 1.0), None);
        assert_eq!(r.on_pointer(56.5, 1.0), Some(RevealChange::Hide));
        assert!(!r.is_revealed());
    }

    #[test]
    fn retina_keeps_the_hysteresis_band_below_the_bar() {
        let mut r = Reveal::default();
        r.on_pointer(1.0, 2.0);
        assert_eq!(bar_px(2.0), 80.0);
        // First pixel under the 80 px bar must not hide it on a 2× display.
        assert_eq!(r.on_pointer(81.0, 2.0), None);
        assert_eq!(r.on_pointer(112.0, 2.0), None);
        assert_eq!(r.on_pointer(113.0, 2.0), Some(RevealChange::Hide));
    }

    #[test]
    fn inset_follows_state_and_scale() {
        let mut r = Reveal::default();
        assert_eq!(r.inset_px(2.0), 0.0);
        r.on_pointer(0.0, 2.0);
        assert_eq!(r.inset_px(2.0), 80.0);
        assert_eq!(r.inset_px(1.25), 50.0);
    }

    #[test]
    fn reset_reports_a_hide_only_when_revealed() {
        let mut r = Reveal::default();
        assert_eq!(r.reset(), None);
        r.on_pointer(0.0, 1.0);
        assert_eq!(r.reset(), Some(RevealChange::Hide));
        assert!(!r.is_revealed());
        assert_eq!(r.reset(), None);
    }

    #[test]
    fn nonsense_scale_falls_back_to_one() {
        assert_eq!(bar_px(0.0), BAR_CSS_PX);
        assert_eq!(bar_px(f64::INFINITY), BAR_CSS_PX);
    }
}
