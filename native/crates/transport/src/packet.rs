//! App-level fragmentation of encoded access units into datagrams, with
//! proactive FEC for keyframes (Plan 04 §6: "encoded frames, app-fragmented ≤
//! ~1200 B datagrams, protected by FEC"; "keyframes / LTR-ack packets get heavy
//! FEC ... delta frames are just dropped").
//!
//! Each datagram is `[header][shard]`. Two sub-formats, distinguished by
//! `recovery_count`:
//!   * `recovery_count == 0` (delta frames): shards are the natural chunk bytes;
//!     the AU is the in-order concatenation once every shard arrives, else the
//!     frame is dropped (no head-of-line blocking).
//!   * `recovery_count > 0` (keyframes): every shard is padded to a fixed length
//!     so Reed–Solomon can regenerate up to `recovery_count` lost originals from
//!     redundancy already in flight — recovery within the frame interval, no
//!     retransmit latency.

use std::collections::HashMap;

use crate::fec;

/// Max application payload per datagram, leaving headroom under str0m's
/// `DATAGRAM_MTU_TARGET` for SCTP/DTLS overhead.
pub const MAX_PAYLOAD: usize = 1100;

const HEADER_LEN: usize = 16;
const FLAG_KEYFRAME: u8 = 0x01;

/// Keyframe FEC redundancy: ~1 recovery shard per 4 originals (25%).
fn recovery_for(keyframe: bool, original_count: usize) -> usize {
    if keyframe {
        original_count.div_ceil(4).max(1)
    } else {
        0
    }
}

/// One wire fragment.
#[derive(Debug, Clone)]
pub struct Fragment(pub Vec<u8>);

#[allow(clippy::too_many_arguments)]
fn write_header(
    buf: &mut Vec<u8>,
    frame_id: u32,
    total_len: u32,
    shard_index: u16,
    original_count: u16,
    recovery_count: u16,
    keyframe: bool,
) {
    buf.extend_from_slice(&frame_id.to_le_bytes());
    buf.extend_from_slice(&total_len.to_le_bytes());
    buf.extend_from_slice(&shard_index.to_le_bytes());
    buf.extend_from_slice(&original_count.to_le_bytes());
    buf.extend_from_slice(&recovery_count.to_le_bytes());
    buf.push(if keyframe { FLAG_KEYFRAME } else { 0 });
    buf.push(0); // reserved
}

/// Split an access unit into ordered fragments, adding FEC recovery shards for
/// keyframes.
pub fn fragment(frame_id: u32, keyframe: bool, au: &[u8]) -> Vec<Fragment> {
    if au.is_empty() {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = au.chunks(MAX_PAYLOAD).collect();
    let n = chunks.len();
    let total_len = au.len() as u32;

    // Try to build FEC recovery for keyframes; fall back to no-FEC if the shard
    // count is unsupported by the codec.
    let want_recovery = recovery_for(keyframe, n);
    if want_recovery > 0 {
        let l = fec::padded_len(MAX_PAYLOAD);
        let padded: Vec<Vec<u8>> = chunks
            .iter()
            .map(|c| {
                let mut v = vec![0u8; l];
                v[..c.len()].copy_from_slice(c);
                v
            })
            .collect();
        if let Ok(recovery) = fec::encode_recovery(&padded, want_recovery) {
            let rc = recovery.len() as u16;
            let mut out = Vec::with_capacity(n + recovery.len());
            for (i, shard) in padded.iter().enumerate() {
                let mut b = Vec::with_capacity(HEADER_LEN + l);
                write_header(
                    &mut b, frame_id, total_len, i as u16, n as u16, rc, keyframe,
                );
                b.extend_from_slice(shard);
                out.push(Fragment(b));
            }
            for (j, shard) in recovery.iter().enumerate() {
                let mut b = Vec::with_capacity(HEADER_LEN + l);
                write_header(
                    &mut b,
                    frame_id,
                    total_len,
                    (n + j) as u16,
                    n as u16,
                    rc,
                    keyframe,
                );
                b.extend_from_slice(shard);
                out.push(Fragment(b));
            }
            return out;
        }
    }

    // No-FEC path: natural chunk sizes.
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut b = Vec::with_capacity(HEADER_LEN + chunk.len());
            write_header(&mut b, frame_id, total_len, i as u16, n as u16, 0, keyframe);
            b.extend_from_slice(chunk);
            Fragment(b)
        })
        .collect()
}

/// A fully reassembled access unit.
#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

struct Partial {
    total_len: u32,
    original_count: u16,
    recovery_count: u16,
    keyframe: bool,
    /// shard payloads indexed 0..original+recovery.
    shards: Vec<Option<Vec<u8>>>,
    originals_have: u16,
    total_have: u16,
}

/// Reassembles fragments into frames, recovering keyframes via FEC and dropping
/// stale incomplete frames so a lost delta frame never stalls the pipeline (§6).
#[derive(Default)]
pub struct Reassembler {
    partials: HashMap<u32, Partial>,
    newest_done: u32,
    have_done: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one received fragment. Returns a frame once it can be assembled
    /// (all originals present, or enough shards to FEC-recover them).
    pub fn push(&mut self, frag: &[u8]) -> Option<ReassembledFrame> {
        if frag.len() < HEADER_LEN {
            return None;
        }
        let frame_id = u32::from_le_bytes(frag[0..4].try_into().ok()?);
        let total_len = u32::from_le_bytes(frag[4..8].try_into().ok()?);
        let shard_index = u16::from_le_bytes(frag[8..10].try_into().ok()?);
        let original_count = u16::from_le_bytes(frag[10..12].try_into().ok()?);
        let recovery_count = u16::from_le_bytes(frag[12..14].try_into().ok()?);
        let keyframe = frag[14] & FLAG_KEYFRAME != 0;
        let payload = frag[HEADER_LEN..].to_vec();

        let total_shards = original_count as usize + recovery_count as usize;
        if original_count == 0 || shard_index as usize >= total_shards {
            return None;
        }
        // Ignore late fragments of an already-superseded frame.
        if self.have_done && frame_id.wrapping_sub(self.newest_done) > u32::MAX / 2 {
            return None;
        }

        let entry = self.partials.entry(frame_id).or_insert_with(|| Partial {
            total_len,
            original_count,
            recovery_count,
            keyframe,
            shards: vec![None; total_shards],
            originals_have: 0,
            total_have: 0,
        });
        let idx = shard_index as usize;
        if entry.shards[idx].is_none() {
            entry.shards[idx] = Some(payload);
            entry.total_have += 1;
            if idx < entry.original_count as usize {
                entry.originals_have += 1;
            }
        }

        let can_assemble = entry.originals_have == entry.original_count
            || (entry.recovery_count > 0 && entry.total_have >= entry.original_count);
        if !can_assemble {
            return None;
        }

        let entry = self.partials.remove(&frame_id).unwrap();
        let data = assemble(
            entry.total_len,
            entry.original_count,
            entry.recovery_count,
            entry.shards,
        )?;
        self.mark_done(frame_id);
        self.partials
            .retain(|&id, _| id.wrapping_sub(frame_id) < u32::MAX / 2);
        Some(ReassembledFrame {
            frame_id,
            keyframe: entry.keyframe,
            data,
        })
    }

    fn mark_done(&mut self, frame_id: u32) {
        if !self.have_done || frame_id.wrapping_sub(self.newest_done) < u32::MAX / 2 {
            self.newest_done = frame_id;
            self.have_done = true;
        }
    }
}

/// Assemble the AU from shards, FEC-recovering missing originals if needed.
///
/// When `recovery_count > 0` the shards are FEC-padded to a fixed length, so
/// each original contributes exactly `MAX_PAYLOAD` real bytes (the last one
/// fewer) — we strip the padding per shard. When `recovery_count == 0` the
/// shards are natural-length and simply concatenate.
fn assemble(
    total_len: u32,
    original_count: u16,
    recovery_count: u16,
    shards: Vec<Option<Vec<u8>>>,
) -> Option<Vec<u8>> {
    let n = original_count as usize;
    let padded = recovery_count > 0;

    // Real byte length contributed by original shard `i`.
    let real_len = |i: usize| -> usize {
        if !padded {
            return usize::MAX; // natural length; use the whole shard
        }
        if i + 1 < n {
            MAX_PAYLOAD
        } else {
            (total_len as usize).saturating_sub((n - 1) * MAX_PAYLOAD)
        }
    };

    // Recover missing originals via FEC if any are absent.
    let mut originals: Vec<Option<Vec<u8>>> = shards.iter().take(n).cloned().collect();
    if originals.iter().any(|s| s.is_none()) {
        if recovery_count == 0 {
            return None;
        }
        let original: Vec<(usize, Vec<u8>)> = shards[..n]
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|v| (i, v.clone())))
            .collect();
        let recovery: Vec<(usize, Vec<u8>)> = shards[n..]
            .iter()
            .enumerate()
            .filter_map(|(j, s)| s.as_ref().map(|v| (j, v.clone())))
            .collect();
        let restored = fec::reconstruct(n, recovery_count as usize, &original, &recovery).ok()?;
        for (i, shard) in restored {
            if i < n && originals[i].is_none() {
                originals[i] = Some(shard);
            }
        }
    }

    let mut data = Vec::with_capacity(total_len as usize);
    for (i, s) in originals.into_iter().enumerate() {
        let shard = s?;
        let take = real_len(i).min(shard.len());
        data.extend_from_slice(&shard[..take]);
    }
    data.truncate(total_len as usize);
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_multi_fragment_delta_frame() {
        let au: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let frags = fragment(7, false, &au);
        assert!(frags.len() >= 3);
        let mut r = Reassembler::new();
        let mut out = None;
        for f in &frags {
            if let Some(frame) = r.push(&f.0) {
                out = Some(frame);
            }
        }
        let frame = out.expect("frame reassembled");
        assert_eq!(frame.frame_id, 7);
        assert!(!frame.keyframe);
        assert_eq!(frame.data, au);
    }

    #[test]
    fn keyframe_recovers_a_lost_fragment_via_fec() {
        // Big enough for several shards + recovery.
        let au: Vec<u8> = (0..8000u32).map(|i| (i * 7) as u8).collect();
        let frags = fragment(42, true, &au);
        // There must be recovery shards.
        assert!(frags.len() > au.len().div_ceil(MAX_PAYLOAD));

        // Drop one original fragment (index 1) — deliver everything else.
        let mut r = Reassembler::new();
        let mut out = None;
        for (i, f) in frags.iter().enumerate() {
            if i == 1 {
                continue; // simulate loss of an original shard
            }
            if let Some(frame) = r.push(&f.0) {
                out = Some(frame);
            }
        }
        let frame = out.expect("keyframe FEC-recovered");
        assert!(frame.keyframe);
        assert_eq!(frame.data, au);
    }

    #[test]
    fn incomplete_delta_frame_is_dropped_when_superseded() {
        let mut r = Reassembler::new();
        let f1 = fragment(1, false, &[9u8; 2000]);
        assert!(r.push(&f1[0].0).is_none());
        let f2 = fragment(2, false, &[5u8; 100]);
        assert!(r.push(&f2[0].0).is_some());
        assert!(r.partials.is_empty());
    }
}
