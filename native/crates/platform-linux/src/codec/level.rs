//! H.264 / HEVC level selection, shared by both hardware encoders.
//!
//! The level goes into the SPS once per IDR, but the transport's bandwidth
//! estimate moves the bitrate live without one. Deriving the level from the
//! *starting* bitrate would let a stream that began at 0.5 Mbps (level 3.0)
//! later run at many times that level's MaxBR — a non-conforming stream that a
//! strict decoder may refuse. So the level is chosen from size and frame rate
//! with a fixed bitrate ceiling the live rate is never expected to exceed
//! (the engine's own target is 8 Mbps).
//!
//! ## References
//! * ITU-T H.264 (08/2021) Annex A, Table A-1 and §A.3.1 (f)/(g).
//! * ITU-T H.265 (09/2023) Annex A, Tables A.8 and A.9 (Main tier),
//!   §A.4.1 (picture width/height ≤ √(8·MaxLumaPs)).

/// The bitrate every declared level must allow.
pub(crate) const BITRATE_CEILING_BPS: u64 = 50_000_000;
// WHY 50 Mbit/s: H.264 level 4.1's MaxBR — so a 1080p30 stream stays at the
// level every H.264 hardware decoder supports.
const _: () = assert!(BITRATE_CEILING_BPS <= 50_000 * 1000);

/// One row of H.264 Table A-1: `(level_idc, MaxMBPS, MaxFS, MaxBR kbit/s)`.
/// MaxBR is the VCL figure for Baseline/Main (cpbBrVclFactor 1000).
const H264_LEVELS: &[(u8, u64, u64, u64)] = &[
    (10, 1_485, 99, 64),
    (11, 3_000, 396, 192),
    (12, 6_000, 396, 384),
    (13, 11_880, 396, 768),
    (20, 11_880, 396, 2_000),
    (21, 19_800, 792, 4_000),
    (22, 20_250, 1_620, 4_000),
    (30, 40_500, 1_620, 10_000),
    (31, 108_000, 3_600, 14_000),
    (32, 216_000, 5_120, 20_000),
    (40, 245_760, 8_192, 20_000),
    (41, 245_760, 8_192, 50_000),
    (42, 522_240, 8_704, 50_000),
    (50, 589_824, 22_080, 135_000),
    (51, 983_040, 36_864, 240_000),
    (52, 2_073_600, 36_864, 240_000),
    (60, 4_177_920, 139_264, 240_000),
    (61, 8_355_840, 139_264, 480_000),
    (62, 16_711_680, 139_264, 800_000),
];

/// One row of H.265 Tables A.8/A.9, Main tier:
/// `(general_level_idc = 30 × level, MaxLumaPs, MaxLumaSr, MaxBR kbit/s)`.
/// (HEVC is encoded by NVENC only.)
#[cfg(feature = "nvenc")]
const HEVC_LEVELS: &[(u32, u64, u64, u64)] = &[
    (30, 36_864, 552_960, 128),
    (60, 122_880, 3_686_400, 1_500),
    (63, 245_760, 7_372_800, 3_000),
    (90, 552_960, 16_588_800, 6_000),
    (93, 983_040, 33_177_600, 10_000),
    (120, 2_228_224, 66_846_720, 12_000),
    (123, 2_228_224, 133_693_440, 20_000),
    (150, 8_912_896, 267_386_880, 25_000),
    (153, 8_912_896, 534_773_760, 40_000),
    (156, 8_912_896, 1_069_547_520, 60_000),
    (180, 35_651_584, 1_069_547_520, 60_000),
    (183, 35_651_584, 2_139_095_040, 120_000),
    (186, 35_651_584, 4_278_190_080, 240_000),
];

/// H.264 `level_idc` (also NVENC's `NV_ENC_LEVEL_H264_*`) for this size and
/// rate at [`BITRATE_CEILING_BPS`].
pub(crate) fn h264_level(width: u32, height: u32, fps_num: u32, fps_den: u32) -> u8 {
    h264_level_at(width, height, fps_num, fps_den, BITRATE_CEILING_BPS)
}

/// The lowest H.264 level whose limits hold the stream: frame size (MaxFS,
/// and each dimension ≤ √(8·MaxFS) in macroblocks), macroblock rate and
/// bitrate. Beyond every limit → the highest level.
fn h264_level_at(width: u32, height: u32, fps_num: u32, fps_den: u32, bitrate_bps: u64) -> u8 {
    let (w_mbs, h_mbs) = (width.div_ceil(16) as u64, height.div_ceil(16) as u64);
    let fs = w_mbs * h_mbs;
    let mbps = (fs * fps_num as u64).div_ceil(fps_den.max(1) as u64);
    let longest = w_mbs.max(h_mbs);
    H264_LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs, max_br)| {
            fs <= max_fs
                && longest * longest <= 8 * max_fs
                && mbps <= max_mbps
                && bitrate_bps <= max_br * 1000
        })
        .or(H264_LEVELS.last())
        .map_or(62, |l| l.0)
}

/// HEVC `general_level_idc`, Main tier (also NVENC's `NV_ENC_LEVEL_HEVC_*`)
/// for this size and rate at [`BITRATE_CEILING_BPS`]. The ceiling puts every
/// stream at level 5.2 or above; Main tier is kept because High-tier decode
/// support is the less common one.
#[cfg(feature = "nvenc")]
pub(crate) fn hevc_level(width: u32, height: u32, fps_num: u32, fps_den: u32) -> u32 {
    hevc_level_at(width, height, fps_num, fps_den, BITRATE_CEILING_BPS)
}

#[cfg(feature = "nvenc")]
fn hevc_level_at(width: u32, height: u32, fps_num: u32, fps_den: u32, bitrate_bps: u64) -> u32 {
    let ps = width as u64 * height as u64;
    let sr = (ps * fps_num as u64).div_ceil(fps_den.max(1) as u64);
    let longest = width.max(height) as u64;
    HEVC_LEVELS
        .iter()
        .find(|&&(_, max_ps, max_sr, max_br)| {
            ps <= max_ps
                && longest * longest <= 8 * max_ps
                && sr <= max_sr
                && bitrate_bps <= max_br * 1000
        })
        .or(HEVC_LEVELS.last())
        .map_or(186, |l| l.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_table_limits() {
        assert_eq!(h264_level_at(1920, 1080, 30, 1, 8_000_000), 40);
        assert_eq!(h264_level_at(1920, 1080, 60, 1, 8_000_000), 42);
        // Bitrate above level 4's 20 Mbit/s pushes to 4.1.
        assert_eq!(h264_level_at(1920, 1080, 30, 1, 30_000_000), 41);
        assert_eq!(h264_level_at(3840, 2160, 30, 1, 20_000_000), 51);
        assert_eq!(h264_level_at(3840, 2160, 60, 1, 20_000_000), 52);
        assert_eq!(h264_level_at(1280, 720, 30, 1, 4_000_000), 31);
        assert_eq!(h264_level_at(7680, 4320, 60, 1, 40_000_000), 61);
        // A 32768-wide strip fits no level's √(8·MaxFS) → highest.
        assert_eq!(h264_level_at(32768, 16, 1, 1, 1), 62);
    }

    #[test]
    fn h264_level_ignores_the_starting_bitrate() {
        // The regression: 640x360 at 0.5 Mbps used to declare level 3.0
        // (MaxBR 10 Mbit/s) and then accept a 100 Mbit/s target live.
        assert_eq!(h264_level(640, 360, 30, 1), 41);
        assert_eq!(h264_level(1280, 720, 30, 1), 41);
        assert_eq!(h264_level(1920, 1080, 30, 1), 41);
        assert_eq!(h264_level(1920, 1080, 60, 1), 42);
        assert_eq!(h264_level(2560, 1440, 60, 1), 51);
        assert_eq!(h264_level(3840, 2160, 60, 1), 52);
    }

    #[cfg(feature = "nvenc")]
    #[test]
    fn hevc_levels() {
        assert_eq!(hevc_level_at(1920, 1080, 30, 1, 8_000_000), 120);
        assert_eq!(hevc_level_at(1920, 1080, 60, 1, 8_000_000), 123);
        assert_eq!(hevc_level_at(3840, 2160, 60, 1, 20_000_000), 153);
        assert_eq!(hevc_level_at(640, 360, 30, 1, 1_000_000), 63);
        // With the ceiling: Main tier needs 5.2 for 50 Mbit/s.
        assert_eq!(hevc_level(1920, 1080, 60, 1), 156);
        assert_eq!(hevc_level(3840, 2160, 60, 1), 156);
        assert_eq!(hevc_level(7680, 4320, 30, 1), 180);
        assert_eq!(hevc_level(7680, 4320, 60, 1), 183);
    }
}
