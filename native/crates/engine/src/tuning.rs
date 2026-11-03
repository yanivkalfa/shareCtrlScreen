//! Session tuning shared by the transport driver and the media threads.
//!
//! ## Congestion policy (V2)
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
//! encode size changes only when the viewer window actually resizes.
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
/// so the driver and the tests agree on exactly one rule.
pub(crate) fn encoder_target(bwe_bps: u32) -> u32 {
    bwe_bps.clamp(MIN_BITRATE, TARGET_BITRATE)
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
}
