//! Session tuning shared by the transport driver and the media threads.
//!
//! ## Congestion policy (V1)
//!
//! The encoder's CBR target FOLLOWS str0m's bandwidth estimate (BWE / Google
//! Congestion Control) down to a low floor, and degradation on a constrained
//! link is spent on **resolution**, not on a high bitrate floor. A high floor
//! (the pre-V1 2.5 Mbps) made the host overshoot a slow link: it kept encoding
//! 2.5 Mbps into a 65 kbps uplink, the overshoot caused loss, GCC collapsed the
//! estimate, and the loop sustained itself ("the mouse stays behind"). The last
//! coherent design before that regression (commit 352d56d) used a 400 kbps floor
//! and let the encoder track the estimate down, metering egress with str0m's own
//! pacer — and it was smooth. This restores that behaviour while keeping text
//! sharp: as the estimate falls we encode a SMALLER image at the same per-pixel
//! quality (a sharp 1/3-scale picture), instead of a mushy full-res one.
//!
//! ## References
//! * str0m 0.21 `bwe` (TWCC/REMB Google Congestion Control): the estimate is
//!   steered toward [`Transport::set_desired_bitrate`] and recovers via str0m's
//!   own padding probes when the link's one-way delay is low, independent of the
//!   current send rate — so a dipped estimate can climb back up as long as the
//!   host stops overshooting and causing loss.
//! * Commit 352d56d ("transport: move video to str0m RTP media path"): the
//!   proven-smooth precedent — `MIN_BITRATE` 400 k, encoder follows BWE down, no
//!   hand-rolled source pacer; str0m's pacer metered egress and frame SIZE (not a
//!   bitrate floor) absorbed congestion.
//!
//! [`Transport::set_desired_bitrate`]: transport::Transport::set_desired_bitrate

/// Bitrate policy (§6). H.264 keeps TEXT sharp with real headroom on a good link
/// (8 Mbps is what the Electron build this rewrite replaced ran at, and it looked
/// right), so that is the target and the ceiling. The FLOOR is deliberately low:
/// the encoder must be free to follow the bandwidth estimate most of the way down
/// instead of overshooting a slow link, because sharpness on a constrained link
/// is preserved by shrinking the ENCODE RESOLUTION (see [`resolution_step`]), not
/// by refusing to lower the bitrate. 500 kbps (just above the proven-smooth
/// 352d56d value of 400 k) comfortably carries the smallest ladder step.
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
/// Never pace the source below this — under a truly bad link we want a slow but
/// still-updating picture, not a slideshow that looks frozen.
pub(crate) const MIN_FPS: u32 = 4;

/// Encode-resolution ladder: the scale applied to the target encode size as the
/// link degrades. Index 0 is full size; each later step is a smaller, still-sharp
/// image. A 600 kbps stream becomes a crisp 1/2- or 1/3-scale picture rather than
/// a blurred full-res one. Degradation is spent here first, and only on frame
/// rate (see [`target_fps`]) once the smallest step still can't fit the link.
pub(crate) const RESOLUTION_LADDER: [f32; 4] = [1.0, 0.75, 0.5, 0.33];

/// Link bitrate (bps) at or below which we step DOWN past boundary `i` (between
/// ladder index `i` and `i + 1`). Monotonically decreasing: a slower link forces
/// a smaller image.
const LADDER_DOWN: [u32; 3] = [4_000_000, 1_500_000, 700_000];
/// Link bitrate (bps) at or above which we step UP past boundary `i`. Each sits
/// ABOVE the matching [`LADDER_DOWN`] threshold; the gap is the hysteresis band
/// that stops the resolution flapping when the estimate jitters around a
/// boundary.
const LADDER_UP: [u32; 3] = [5_000_000, 2_000_000, 1_000_000];

/// The encoder's CBR target for a given bandwidth estimate: the estimate clamped
/// into `[MIN_BITRATE, TARGET_BITRATE]`. Below the floor we cannot usefully lower
/// the bitrate further (quality is held by [`resolution_step`] instead), and
/// above the ceiling there is nothing to gain. Pure so the driver and the tests
/// agree on exactly one rule.
pub(crate) fn encoder_target(bwe_bps: u32) -> u32 {
    bwe_bps.clamp(MIN_BITRATE, TARGET_BITRATE)
}

/// The resolution-ladder index to use given the current index `current` and the
/// raw link estimate `link_bps`, with hysteresis (see [`LADDER_DOWN`] /
/// [`LADDER_UP`]). Monotonic: for a fixed `current`, a higher `link_bps` never
/// returns a larger (smaller-image) index. A sudden drop may skip several steps;
/// recovery and flap-resistance come from the down/up gap, not from single
/// stepping.
pub(crate) fn resolution_step(current: usize, link_bps: u32) -> usize {
    let last = RESOLUTION_LADDER.len() - 1;
    let mut idx = current.min(last);
    // Step down while the link is at/below the down-threshold of the current
    // boundary. `<=` so a link exactly at the threshold takes the smaller image.
    while idx < LADDER_DOWN.len() && link_bps <= LADDER_DOWN[idx] {
        idx += 1;
    }
    // Step up while the link is at/above the up-threshold of the boundary above.
    // Because every up-threshold exceeds its down-threshold, this can never fire
    // in the same call as the loop above, so the result is stable.
    while idx > 0 && link_bps >= LADDER_UP[idx - 1] {
        idx -= 1;
    }
    idx
}

/// The capture/encode frame rate for a given ladder index and link estimate.
/// Above the smallest ladder step we always run at [`MAX_FPS`] and let resolution
/// absorb congestion. Only at the smallest step, when the link cannot carry even
/// that image at [`MIN_BITRATE`], do we trade frame RATE (the last resort), so
/// egress matches the link instead of overshooting it. Scales down with the link
/// and never below [`MIN_FPS`], so a recovering link ramps the rate back up.
pub(crate) fn target_fps(step: usize, link_bps: u32) -> u32 {
    let bottom = RESOLUTION_LADDER.len() - 1;
    if step < bottom || link_bps >= MIN_BITRATE {
        return MAX_FPS;
    }
    // link_bps < MIN_BITRATE: run proportionally slower, floored at MIN_FPS.
    let scaled = (u64::from(MAX_FPS) * u64::from(link_bps) / u64::from(MIN_BITRATE)) as u32;
    scaled.clamp(MIN_FPS, MAX_FPS)
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

    #[test]
    fn resolution_ladder_steps_down_as_the_link_falls() {
        // From full res, each band selects a progressively smaller image.
        assert_eq!(resolution_step(0, 8_000_000), 0); // plenty: full
        assert_eq!(resolution_step(0, 3_000_000), 1); // below 4M down-threshold
        assert_eq!(resolution_step(0, 1_000_000), 2); // below 1.5M
        assert_eq!(resolution_step(0, 600_000), 3); // below 700k: smallest
                                                    // A sudden collapse may skip straight to the bottom.
        assert_eq!(resolution_step(0, 65_000), 3);
    }

    #[test]
    fn resolution_ladder_is_hysteretic() {
        // Sitting at 0.75 (index 1): a link in the [1.5M, 2M) band holds there —
        // it must not climb back to full until it clears the 2M… wait, 5M up for
        // boundary 0. A link at 4.5M (between down 4M and up 5M for boundary 0)
        // holds at index 1 rather than flapping to full.
        assert_eq!(resolution_step(1, 4_500_000), 1);
        // Clear the up-threshold and it climbs.
        assert_eq!(resolution_step(1, 5_000_000), 0);
        // At 0.5 (index 2), the [1.5M, 2M) band holds; 2M climbs to 0.75.
        assert_eq!(resolution_step(2, 1_800_000), 2);
        assert_eq!(resolution_step(2, 2_000_000), 1);
        // At 0.5, the down band of the step below keeps it from dropping early.
        assert_eq!(resolution_step(2, 800_000), 2);
        assert_eq!(resolution_step(2, 700_000), 3);
    }

    #[test]
    fn resolution_step_is_monotonic_in_the_link() {
        // For any fixed current index, a higher link never yields a larger index.
        for current in 0..RESOLUTION_LADDER.len() {
            let mut prev = usize::MAX;
            for link in (0..=9_000_000).step_by(50_000) {
                let idx = resolution_step(current, link);
                assert!(
                    idx <= prev || prev == usize::MAX,
                    "current={current} link={link}"
                );
                prev = idx;
            }
        }
    }

    #[test]
    fn fps_drops_only_below_the_smallest_step_and_recovers() {
        let bottom = RESOLUTION_LADDER.len() - 1;
        // Above the bottom step, always full rate regardless of the link.
        assert_eq!(target_fps(0, 100_000), MAX_FPS);
        assert_eq!(target_fps(1, 100_000), MAX_FPS);
        // At the bottom step, a link above the floor still runs full rate.
        assert_eq!(target_fps(bottom, MIN_BITRATE), MAX_FPS);
        assert_eq!(target_fps(bottom, 600_000), MAX_FPS);
        // Below the floor, the rate scales down but never below MIN_FPS.
        assert_eq!(target_fps(bottom, 250_000), 15); // 30 * 250k/500k
        assert_eq!(target_fps(bottom, 0), MIN_FPS);
        assert!(target_fps(bottom, 10_000) >= MIN_FPS);
        // Monotonic non-decreasing as the link recovers.
        let mut prev = 0;
        for link in (0..=MIN_BITRATE).step_by(10_000) {
            let f = target_fps(bottom, link);
            assert!(f >= prev);
            prev = f;
        }
    }
}
