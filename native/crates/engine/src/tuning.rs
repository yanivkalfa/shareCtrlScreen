//! Session tuning shared by the transport driver and the media threads.
//!
//! ## Congestion policy (V3)
//!
//! The encoder's CBR target FOLLOWS str0m's bandwidth estimate (BWE / Google
//! Congestion Control) down to a low floor and back up, and that is the ONLY thing
//! the link estimate steers. A high floor (the pre-V1 2.5 Mbps) made the host
//! overshoot a slow link: it kept encoding 2.5 Mbps into a 65 kbps uplink, the
//! overshoot caused loss, GCC collapsed the estimate, and the loop sustained itself
//! ("the mouse stays behind"). The last coherent design before that regression
//! (commit 352d56d) used a low floor and let the encoder track the estimate down,
//! metering egress with str0m's own pacer — and it was smooth. This keeps that
//! behaviour: no hand-rolled source pacer, the encoder follows BWE down, str0m's
//! pacer meters egress.
//!
//! ### V3: the applied target is SMOOTHED, not the raw estimate
//!
//! V2 fed the encoder the raw estimate (clamped) with a weak 15% hysteresis. On a
//! capable but noisy link GCC's estimate swings WILDLY — the owner's live host log
//! showed it bouncing 4574→3000→1432→692→4130→500 kbps within ~2 minutes — and the
//! encoder bitrate followed it 1:1, so the full-resolution picture PULSED
//! blurry→sharp→blurry every few seconds (sharp at ~4 Mbps, blocky at ~500 kbps).
//! The frequent reconfigs also made Media Foundation emit 1–4 keyframes per 5 s
//! despite a 600-frame GOP, and each IDR burst destabilised GCC further — a
//! feedback loop.
//!
//! V3 replaces the raw-follow with [`RateFilter`]: a light exponential moving
//! average (EMA) of the estimate, plus ASYMMETRIC sustained-window gating on the
//! applied target. The target only moves after the smoothed estimate stays out of
//! a dead-band CONTINUOUSLY for a hold window (longer to drop than to raise), so
//! transient dips and single-sample spikes never reconfigure the encoder; a
//! GENUINE sustained trend still moves it within a few seconds. The result: the
//! applied bitrate is STEADY on a noisy-but-stable link and the encoder is
//! reconfigured rarely, which also cuts the reconfig→keyframe feedback.
//!
//! ## No resolution ladder (V2 removes V1's)
//!
//! V1 (commit 003fbf8) additionally scaled the ENCODE RESOLUTION down a
//! bandwidth-driven ladder, meaning to spend degradation on a sharp smaller image.
//! In practice a noisy or even merely-good estimate scaled the picture down and
//! oscillated between ladder steps, so the image CONSTANTLY softened and
//! re-sharpened — the owner's "ever blurring and focusing on things that don't
//! change", unusable on every platform including a good Windows link. V2 removes
//! the ladder entirely: the encoder ALWAYS encodes at the full viewer-fit
//! resolution, and a constrained link is absorbed purely by the bitrate following
//! BWE down (a softer but STEADY picture), never by changing the encode size. The
//! encode size changes only when the viewer window actually resizes. V3 keeps this
//! unchanged — it only smooths HOW the bitrate follows BWE.
//!
//! ## References
//! * str0m 0.21 `bwe` (TWCC/REMB Google Congestion Control): the estimate is
//!   steered toward [`Transport::set_desired_bitrate`] and recovers via str0m's
//!   own padding probes when the link's one-way delay is low, independent of the
//!   current send rate — so a dipped estimate can climb back up as long as the
//!   host stops overshooting and causing loss.
//! * Commit 352d56d ("transport: move video to str0m RTP media path"): the
//!   proven-smooth precedent — a low `MIN_BITRATE`, encoder follows BWE down, no
//!   hand-rolled source pacer; str0m's pacer metered egress.
//!
//! [`Transport::set_desired_bitrate`]: transport::Transport::set_desired_bitrate

/// Bitrate policy (§6). H.264 keeps TEXT sharp with real headroom on a good link
/// (8 Mbps is what the Electron build this rewrite replaced ran at, and it looked
/// right), so that is the target and the ceiling. The FLOOR is deliberately low:
/// the encoder must be free to follow the bandwidth estimate most of the way down
/// instead of overshooting a slow link. A constrained link gets a softer but
/// STEADY full-resolution picture — the encode size never changes with the link,
/// only the bitrate does (see [`encoder_target`]). 500 kbps (just above the
/// proven-smooth 352d56d value of 400 k) keeps even a low link legible.
pub(crate) const TARGET_BITRATE: u32 = 8_000_000;
pub(crate) const MIN_BITRATE: u32 = 500_000;
/// What we ask the BWE to probe toward. Kept at the target (not above it): asking
/// for more only pads the link with probe traffic, which on a capacity-limited
/// relay is itself a loss source — the pre-V1 9 Mbps was over-aggressive. The
/// target is already the ceiling we ever want, so probing toward it is enough.
pub(crate) const DESIRED_BITRATE: u32 = TARGET_BITRATE;

/// Host encode frame-rate cap (30fps). NOTE: `EncoderConfig::fps_num` MUST match
/// this — CBR budgets bits per DECLARED frame, so a mismatch silently halves
/// both per-frame quality and the bitrate actually used.
pub(crate) const MAX_FPS: u32 = 30;
/// The minimum spacing between encoded frames — we always run at [`MAX_FPS`]. The
/// pacing sleep bites only when the encoder would otherwise run FASTER than this
/// (a busy screen); a slow software encoder or an idle screen sets the real rate.
pub(crate) const MAX_FPS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1000 / MAX_FPS as u64);

/// The encoder's CBR target for a given bandwidth estimate: the estimate clamped
/// into `[MIN_BITRATE, TARGET_BITRATE]`. Below the floor the picture stays full
/// resolution and simply softens; above the ceiling there is nothing to gain. Pure
/// so the driver and the tests agree on exactly one rule. [`RateFilter`] clamps the
/// SMOOTHED estimate through this; the raw estimate is never applied directly.
pub(crate) fn encoder_target(bwe_bps: u32) -> u32 {
    bwe_bps.clamp(MIN_BITRATE, TARGET_BITRATE)
}

// --- V3 smoothing constants (tuned against the owner's live host log) ----------
//
// The log swung bwe 4574→3892→3000→3597→1432→1206→3693→1743→692→936→782→4130→
// 3221→500 kbps within ~2 min — individual dips to ~500–900 kbps that RECOVERED
// within a second or two, on a link that was really ~3–4 Mbps capable. The goal:
// ignore those transient dips entirely, follow only a trend that PERSISTS.

/// EMA time constant for the raw estimate (seconds). A light average: it removes
/// single-sample estimate spikes/notches (str0m emits ~1–2 estimates/s) without
/// lagging a real trend. Kept short because the sustained-window gate below — not
/// this EMA — is what rejects multi-second transients; the EMA only de-noises the
/// signal the gate watches.
const SMOOTH_TAU_SECS: f64 = 1.5;
/// How far (fraction of the CURRENT applied target) the smoothed estimate must sit
/// OUTSIDE the target before the hold timer even starts. V2's 15% reacted to
/// nearly every swing; a ±25% dead-band means ordinary link noise never qualifies.
const DEAD_BAND_FRAC: f64 = 0.25;
/// The smoothed estimate must stay BELOW the dead-band continuously this long
/// before the target drops — longer than any transient dip in the log (~1–2 s),
/// short enough that a genuine sustained congestion is followed within a few
/// seconds (the "don't over-damp a really slow link" / V1 regression risk).
const DROP_HOLD_SECS: f64 = 3.0;
/// The smoothed estimate must stay ABOVE the dead-band continuously this long
/// before the target rises. Shorter than the drop hold: raising quality back is
/// safe (we only raise when BWE itself sustains high, so the capacity is real) and
/// desirable, but still gated so a transient spike can't bump it.
const RAISE_HOLD_SECS: f64 = 2.0;

/// Smooths the encoder's CBR target against bandwidth-estimate noise.
///
/// PURE and timer-free: [`RateFilter::next`] takes the raw estimate, the elapsed
/// wall-clock seconds since the previous sample, and the prior state, and returns
/// the new state. Because the EMA weight is `1 - exp(-dt/τ)` it depends only on
/// elapsed time, not on how OFTEN it is called — so the real path (once per str0m
/// estimate, ~1–2/s) and the test path (re-applied every driver tick) converge on
/// the same trajectory. The tests drive it directly with fixed `dt`, no clocks.
///
/// Policy:
/// * `smoothed` is a light EMA of the raw estimate (kills single-sample spikes).
/// * the applied `target` only changes after `smoothed` (clamped to the bitrate
///   range) stays OUTSIDE a ±[`DEAD_BAND_FRAC`] band around the current target for
///   a continuous hold window — [`DROP_HOLD_SECS`] to lower, [`RAISE_HOLD_SECS`]
///   to raise. Any sample that falls back inside the band resets both timers, so a
///   transient dip/spike never moves the target.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateFilter {
    /// The applied CBR target (bps) — what the encoder is actually reconfigured to.
    target: u32,
    /// EMA of the raw estimate (bps), clamped into the bitrate range on read.
    smoothed: f64,
    /// Continuous seconds the smoothed estimate has sat below the drop dead-band.
    below_secs: f64,
    /// Continuous seconds the smoothed estimate has sat above the raise dead-band.
    above_secs: f64,
}

impl RateFilter {
    /// Start from a known applied target (the initial encoder bitrate), with the
    /// EMA seeded there so the first samples don't lurch.
    pub(crate) fn new(initial_target: u32) -> Self {
        let t = encoder_target(initial_target);
        Self {
            target: t,
            smoothed: t as f64,
            below_secs: 0.0,
            above_secs: 0.0,
        }
    }

    /// The current applied CBR target (bps).
    pub(crate) fn target(&self) -> u32 {
        self.target
    }

    /// Fold one raw estimate (bps) sampled `dt_secs` after the previous one into
    /// the filter and return the updated state. Pure: no clocks, no I/O.
    pub(crate) fn next(mut self, raw_bps: u32, dt_secs: f64) -> Self {
        // Guard against a zero/negative/absurd dt (first sample, clock quirks).
        let dt = dt_secs.clamp(0.0, 5.0);

        // 1) Light EMA of the raw estimate. alpha = 1 - exp(-dt/τ): the weight of
        //    the new sample grows with elapsed time, so the smoothed curve is the
        //    same whether we sample fast (every tick) or slow (every estimate).
        let alpha = 1.0 - (-dt / SMOOTH_TAU_SECS).exp();
        self.smoothed += alpha * (raw_bps as f64 - self.smoothed);

        // 2) Compare the clamped smoothed estimate to the current target.
        let est = (self.smoothed as u32).clamp(MIN_BITRATE, TARGET_BITRATE);
        let target_f = self.target as f64;
        let drop_edge = target_f * (1.0 - DEAD_BAND_FRAC);
        let raise_edge = target_f * (1.0 + DEAD_BAND_FRAC);

        if (est as f64) < drop_edge {
            // Below the band: accumulate toward a drop, forget any rise.
            self.below_secs += dt;
            self.above_secs = 0.0;
        } else if (est as f64) > raise_edge {
            // Above the band: accumulate toward a raise, forget any drop.
            self.above_secs += dt;
            self.below_secs = 0.0;
        } else {
            // Inside the dead-band: the trend didn't persist — reset both.
            self.below_secs = 0.0;
            self.above_secs = 0.0;
        }

        // 3) Commit a change only once a trend has PERSISTED for its hold window.
        //    On commit, snap the target to the smoothed value and reset the timers
        //    (the next change needs a fresh, sustained departure from the new
        //    target — this is what keeps the encoder from being reconfigured on
        //    every swing).
        if self.below_secs >= DROP_HOLD_SECS {
            self.target = est;
            self.below_secs = 0.0;
        } else if self.above_secs >= RAISE_HOLD_SECS {
            self.target = est;
            self.above_secs = 0.0;
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_target_tracks_bwe_but_clamps() {
        // The regression: a 65 kbps link must NOT pin the target at an old high
        // floor — it tracks down to MIN_BITRATE and no further.
        assert_eq!(encoder_target(65_000), MIN_BITRATE);
        assert_eq!(encoder_target(400_000), MIN_BITRATE);
        // In range it tracks the estimate exactly.
        assert_eq!(encoder_target(3_000_000), 3_000_000);
        assert_eq!(encoder_target(MIN_BITRATE), MIN_BITRATE);
        assert_eq!(encoder_target(TARGET_BITRATE), TARGET_BITRATE);
        // Above the ceiling it caps.
        assert_eq!(encoder_target(50_000_000), TARGET_BITRATE);
        // Monotonic non-decreasing in the estimate.
        let mut prev = 0;
        for bwe in (0..=10_000_000).step_by(123_457) {
            let t = encoder_target(bwe);
            assert!(t >= prev);
            prev = t;
        }
    }

    /// Drive the filter for `secs` at `dt`, flipping the raw estimate between
    /// `lo`/`hi` every `half_period` seconds, and return (filter, number of times
    /// the applied target changed).
    fn oscillate(
        mut f: RateFilter,
        lo: u32,
        hi: u32,
        half_period: f64,
        secs: f64,
        dt: f64,
    ) -> (RateFilter, u32) {
        let mut t = 0.0;
        let mut changes = 0u32;
        let mut high = false;
        let mut next_flip = half_period;
        while t < secs {
            if t >= next_flip {
                high = !high;
                next_flip += half_period;
            }
            let prev = f.target();
            f = f.next(if high { hi } else { lo }, dt);
            if f.target() != prev {
                changes += 1;
            }
            t += dt;
        }
        (f, changes)
    }

    /// Drive the filter holding a single raw estimate for `secs`.
    fn hold(mut f: RateFilter, raw: u32, secs: f64, dt: f64) -> RateFilter {
        let mut t = 0.0;
        while t < secs {
            f = f.next(raw, dt);
            t += dt;
        }
        f
    }

    #[test]
    fn noise_in_steady_out() {
        // The real-log pattern: a ~3 Mbps link whose estimate stabs down to ~300k
        // and back to ~4M roughly every second. The applied target must NOT chase
        // it — it should barely move.
        let start = RateFilter::new(2_150_000); // near the oscillation's mean
        let (f, changes) = oscillate(start, 300_000, 4_000_000, 1.0, 20.0, 0.05);
        assert!(
            changes <= 2,
            "applied target pulsed with the noise ({changes} changes in 20 s)"
        );
        // Whatever it settled on stays in a sane mid-band, never cratering to the
        // floor or pinning the ceiling on the transients.
        assert!(
            (1_200_000..=3_500_000).contains(&f.target()),
            "target drifted out of band under noise: {} bps",
            f.target()
        );
    }

    #[test]
    fn sustained_drop_is_followed() {
        // Start high, then the link genuinely drops to ~800 kbps and STAYS there.
        // The target must come down (within a few seconds, not instantly).
        let f = RateFilter::new(4_000_000);
        let f = hold(f, 800_000, 20.0, 0.05);
        assert!(
            f.target() <= 1_200_000,
            "target did not follow a sustained drop: {} bps",
            f.target()
        );
        assert!(f.target() >= MIN_BITRATE);
    }

    #[test]
    fn sustained_rise_is_followed() {
        // Start low (floor), then the link genuinely opens up to ~6 Mbps and STAYS.
        let f = RateFilter::new(MIN_BITRATE);
        let f = hold(f, 6_000_000, 20.0, 0.05);
        assert!(
            f.target() >= 4_000_000,
            "target did not follow a sustained rise: {} bps",
            f.target()
        );
        assert!(f.target() <= TARGET_BITRATE);
    }

    #[test]
    fn a_brief_dip_does_not_move_the_target() {
        // A single ~1 s dip to the floor on an otherwise steady 4 Mbps link must
        // leave the target untouched — this is the exact transient the owner saw.
        let f = RateFilter::new(4_000_000);
        let f = hold(f, 4_000_000, 2.0, 0.05); // steady
        let before = f.target();
        let f = hold(f, 400_000, 1.0, 0.05); // brief dip (< DROP_HOLD)
        let f = hold(f, 4_000_000, 2.0, 0.05); // recovered
        assert_eq!(
            f.target(),
            before,
            "a sub-hold dip moved the target (should be ignored)"
        );
    }

    #[test]
    fn target_stays_in_range() {
        // Even hammered with absurd values the target stays clamped.
        let mut f = RateFilter::new(TARGET_BITRATE);
        for &v in &[0u32, u32::MAX, 50_000_000, 1, 9_000_000] {
            f = hold(f, v, 10.0, 0.1);
            assert!(f.target() >= MIN_BITRATE && f.target() <= TARGET_BITRATE);
        }
    }
}
