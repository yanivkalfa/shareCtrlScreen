//! Session tuning shared by the transport driver and the media threads.

/// Bitrate policy (§6). H.264 needs real headroom to keep TEXT sharp — the
/// Electron build this rewrite replaced ran at 8 Mbps and looked right; the 3
/// Mbps "safe baseline" tried here was a large part of why the native build
/// looked like mush. The floor is deliberately high for the same reason: below
/// ~2.5 Mbps a 1080p desktop cannot stay legible, so when the link can't carry
/// that we spend FRAMES instead of sharpness (see the source pacer).
pub(crate) const TARGET_BITRATE: u32 = 8_000_000;
pub(crate) const MIN_BITRATE: u32 = 2_500_000;
/// What we ask the BWE to probe toward — just above TARGET so it can confirm
/// the link sustains it, without flooding a cellular uplink with padding.
pub(crate) const DESIRED_BITRATE: u32 = 9_000_000;

/// Host encode frame-rate cap (30fps). NOTE: `EncoderConfig::fps_num` MUST match
/// this — CBR budgets bits per DECLARED frame, so a mismatch silently halves
/// both per-frame quality and the bitrate actually used.
pub(crate) const MAX_FPS: u32 = 30;
pub(crate) const MAX_FPS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1000 / MAX_FPS as u64);
/// Never pace the source below this — under a truly bad link we want a slow but
/// still-updating picture, not a slideshow that looks frozen.
pub(crate) const MIN_FPS: u32 = 4;
