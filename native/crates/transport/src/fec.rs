//! Proactive forward error correction (Plan 04 §6) using `reed-solomon-simd`
//! (1.6–10 GiB/s, runtime SIMD). Delta video frames are just dropped on loss;
//! **keyframes / LTR-ack packets get heavy FEC** so they recover within the
//! frame interval from redundancy already in flight — no retransmit latency, no
//! jitter buffer.
//!
//! `reed-solomon-simd` needs equal-length shards whose length is a multiple of
//! 64 bytes, so shards are zero-padded to a 64-byte boundary; the real payload
//! length travels in the packet header (see [`crate::packet`]).

use reed_solomon_simd::{decode, encode};

#[derive(Debug, thiserror::Error)]
pub enum FecError {
    #[error("reed-solomon: {0}")]
    Rs(String),
    #[error("shard count/size unsupported by the codec")]
    Unsupported,
}

/// Round a shard length up to the 64-byte granularity the codec requires.
pub fn padded_len(len: usize) -> usize {
    len.div_ceil(64) * 64
}

/// Generate `recovery_count` recovery shards for `original` (each already padded
/// to an equal, 64-byte-multiple length). Returns the recovery shards in order.
pub fn encode_recovery(
    original: &[Vec<u8>],
    recovery_count: usize,
) -> Result<Vec<Vec<u8>>, FecError> {
    if original.is_empty() || recovery_count == 0 {
        return Ok(Vec::new());
    }
    encode(original.len(), recovery_count, original).map_err(|e| match e {
        reed_solomon_simd::Error::UnsupportedShardCount { .. } => FecError::Unsupported,
        other => FecError::Rs(other.to_string()),
    })
}

/// Reconstruct the missing original shards from whatever survived. `original`
/// and `recovery` are `(index, shard)` pairs for the shards actually received.
/// Returns `(index, shard)` for the shards that had to be restored.
pub fn reconstruct(
    original_count: usize,
    recovery_count: usize,
    original: &[(usize, Vec<u8>)],
    recovery: &[(usize, Vec<u8>)],
) -> Result<Vec<(usize, Vec<u8>)>, FecError> {
    let restored = decode(
        original_count,
        recovery_count,
        original.iter().map(|(i, s)| (*i, s.as_slice())),
        recovery.iter().map(|(i, s)| (*i, s.as_slice())),
    )
    .map_err(|e| FecError::Rs(e.to_string()))?;
    Ok(restored.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_dropped_shard() {
        // 4 original shards, 2 recovery — tolerate up to 2 losses.
        let shard_len = padded_len(50);
        let original: Vec<Vec<u8>> = (0..4)
            .map(|i| {
                let mut v = vec![0u8; shard_len];
                v.iter_mut()
                    .enumerate()
                    .for_each(|(j, b)| *b = (i * 31 + j) as u8);
                v
            })
            .collect();

        let recovery = encode_recovery(&original, 2).unwrap();
        assert_eq!(recovery.len(), 2);

        // Drop original shards 1 and 3; feed survivors + recovery.
        let survived: Vec<(usize, Vec<u8>)> =
            vec![(0, original[0].clone()), (2, original[2].clone())];
        let recv: Vec<(usize, Vec<u8>)> = recovery
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.clone()))
            .collect();

        let restored = reconstruct(4, 2, &survived, &recv).unwrap();
        // Indices 1 and 3 must come back, byte-identical.
        let get = |idx: usize| {
            restored
                .iter()
                .find(|(i, _)| *i == idx)
                .map(|(_, s)| s.clone())
        };
        assert_eq!(get(1).unwrap(), original[1]);
        assert_eq!(get(3).unwrap(), original[3]);
    }
}
