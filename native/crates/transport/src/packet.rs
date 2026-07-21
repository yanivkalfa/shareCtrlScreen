//! App-level fragmentation of encoded access units into datagrams (Plan 04 §6:
//! "encoded frames, app-fragmented ≤ ~1200 B datagrams, protected by FEC").
//! str0m/SCTP will not fragment for us on the unreliable channel, so we chunk
//! each AU ourselves and reassemble on the viewer, dropping any AU whose
//! fragments do not all arrive within the frame interval (delta frames are
//! disposable; keyframes are FEC-protected upstream).

use std::collections::HashMap;

/// Max application payload per datagram, leaving headroom under str0m's
/// `DATAGRAM_MTU_TARGET` for SCTP/DTLS overhead.
pub const MAX_PAYLOAD: usize = 1100;

const HEADER_LEN: usize = 10;
const FLAG_KEYFRAME: u8 = 0x01;

/// One wire fragment: `[frame_id u32][frag_index u16][frag_count u16][flags u8]
/// [reserved u8][payload…]`. Little-endian.
#[derive(Debug, Clone)]
pub struct Fragment(pub Vec<u8>);

/// Split an access unit into ordered fragments tagged with `frame_id`.
pub fn fragment(frame_id: u32, keyframe: bool, au: &[u8]) -> Vec<Fragment> {
    if au.is_empty() {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = au.chunks(MAX_PAYLOAD).collect();
    let count = chunks.len() as u16;
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut buf = Vec::with_capacity(HEADER_LEN + chunk.len());
            buf.extend_from_slice(&frame_id.to_le_bytes());
            buf.extend_from_slice(&(i as u16).to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.push(flags);
            buf.push(0); // reserved
            buf.extend_from_slice(chunk);
            Fragment(buf)
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
    count: u16,
    keyframe: bool,
    parts: Vec<Option<Vec<u8>>>,
    have: u16,
}

/// Reassembles fragments into frames, discarding stale incomplete frames so a
/// lost delta frame never stalls the pipeline (no head-of-line blocking, §6).
#[derive(Default)]
pub struct Reassembler {
    partials: HashMap<u32, Partial>,
    /// Highest completed frame id, to drop late fragments of old frames.
    newest_done: u32,
    have_done: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one received fragment. Returns a frame once all its parts arrive.
    pub fn push(&mut self, frag: &[u8]) -> Option<ReassembledFrame> {
        if frag.len() < HEADER_LEN {
            return None;
        }
        let frame_id = u32::from_le_bytes(frag[0..4].try_into().ok()?);
        let idx = u16::from_le_bytes(frag[4..6].try_into().ok()?);
        let count = u16::from_le_bytes(frag[6..8].try_into().ok()?);
        let keyframe = frag[8] & FLAG_KEYFRAME != 0;
        let payload = &frag[HEADER_LEN..];

        if count == 0 || idx >= count {
            return None;
        }
        // Ignore late fragments of an already-superseded frame.
        if self.have_done && frame_id.wrapping_sub(self.newest_done) > u32::MAX / 2 {
            return None;
        }

        let entry = self.partials.entry(frame_id).or_insert_with(|| Partial {
            count,
            keyframe,
            parts: vec![None; count as usize],
            have: 0,
        });
        if entry.parts[idx as usize].is_none() {
            entry.parts[idx as usize] = Some(payload.to_vec());
            entry.have += 1;
        }
        if entry.have == entry.count {
            let entry = self.partials.remove(&frame_id).unwrap();
            let mut data = Vec::new();
            for p in entry.parts {
                data.extend_from_slice(&p.unwrap());
            }
            self.mark_done(frame_id);
            // Drop any older still-incomplete frames — they're too late to help.
            self.partials
                .retain(|&id, _| id.wrapping_sub(frame_id) < u32::MAX / 2);
            return Some(ReassembledFrame {
                frame_id,
                keyframe: entry.keyframe,
                data,
            });
        }
        None
    }

    fn mark_done(&mut self, frame_id: u32) {
        if !self.have_done || frame_id.wrapping_sub(self.newest_done) < u32::MAX / 2 {
            self.newest_done = frame_id;
            self.have_done = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_multi_fragment_frame() {
        let au: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let frags = fragment(7, true, &au);
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
        assert!(frame.keyframe);
        assert_eq!(frame.data, au);
    }

    #[test]
    fn incomplete_frame_is_dropped_when_superseded() {
        let mut r = Reassembler::new();
        // Frame 1: send only fragment 0 of 2 (never completes).
        let f1 = fragment(1, false, &vec![9u8; 2000]);
        assert!(r.push(&f1[0].0).is_none());
        // Frame 2 completes fully.
        let f2 = fragment(2, false, &vec![5u8; 100]);
        assert!(r.push(&f2[0].0).is_some());
        // The stale partial for frame 1 was evicted.
        assert!(r.partials.is_empty());
    }
}
