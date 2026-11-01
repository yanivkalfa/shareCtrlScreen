//! RBSP extraction and the bit reader the header parsers run on.
//!
//! Every read is bounds-checked and returns [`Bad::Malformed`] past the end:
//! the bytes come from the network, so a truncated or hostile NAL unit must
//! fail the picture, never panic or read out of bounds.

use super::Bad;

/// The RBSP of a NAL unit (header byte included, start code excluded) with the
/// emulation-prevention bytes removed (§7.4.1: `00 00 03 0x` → `00 00 0x`).
///
/// WHY the header byte stays in: VA-API's `slice_data_bit_offset` counts from
/// the start of the NAL unit, header included, in the unescaped domain — so a
/// reader over this buffer reports exactly that offset.
pub(crate) fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Big-endian bit reader over an RBSP.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Bits consumed so far.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bits read so far (the slice-data bit offset after a slice header).
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn flag(&mut self) -> Result<bool, Bad> {
        let byte = self
            .data
            .get(self.pos / 8)
            .ok_or_else(|| Bad::Malformed("header ends early".into()))?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    /// `u(n)`, `n` ≤ 32.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32, Bad> {
        debug_assert!(n <= 32);
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.flag()? as u64;
        }
        Ok(v as u32)
    }

    /// `ue(v)` (§9.1). Codes longer than 32 bits cannot carry any value the
    /// syntax allows (the largest, `bit_rate_value_minus1`, fits 32 bits).
    pub(crate) fn ue(&mut self) -> Result<u32, Bad> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Bad::Malformed("Exp-Golomb code longer than 32 bits".into()));
            }
        }
        let suffix = self.bits(zeros)? as u64;
        Ok(((1u64 << zeros) - 1 + suffix) as u32)
    }

    /// `ue(v)` that must not exceed `max` (the range the semantics allow).
    pub(crate) fn ue_max(&mut self, max: u32, what: &str) -> Result<u32, Bad> {
        let v = self.ue()?;
        if v > max {
            return Err(Bad::Malformed(format!(
                "{what} = {v}, over its limit {max}"
            )));
        }
        Ok(v)
    }

    /// `se(v)` (§9.1.1): 0, 1, 2, 3, 4 … → 0, 1, −1, 2, −2 …
    pub(crate) fn se(&mut self) -> Result<i32, Bad> {
        let k = self.ue()? as i64;
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(v as i32)
    }

    /// `se(v)` within `min..=max`.
    pub(crate) fn se_range(&mut self, min: i32, max: i32, what: &str) -> Result<i32, Bad> {
        let v = self.se()?;
        if !(min..=max).contains(&v) {
            return Err(Bad::Malformed(format!(
                "{what} = {v}, outside {min}..={max}"
            )));
        }
        Ok(v)
    }

    /// `more_rbsp_data()` (§7.2): true while something other than the
    /// `rbsp_trailing_bits` (a stop bit, then zero bits/bytes) is left.
    pub(crate) fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        // The stop bit is the last 1 bit of the last non-zero byte.
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.pos < stop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_reads_the_spec_table() {
        // §9.1 Table 9-2: 1 010 011 00100 0001000, then stop bit + padding —
        // the same bytes `h264::BitWriter` is pinned to.
        let bytes = [0xa6, 0x41, 0x10];
        let mut r = BitReader::new(&bytes);
        for v in [0, 1, 2, 3, 7] {
            assert_eq!(r.ue().expect("ue"), v);
        }
        assert!(!r.more_rbsp_data());
    }

    #[test]
    fn signed_mapping() {
        // ue 0..=4 → se 0, 1, −1, 2, −2: 1 010 011 00100 00101 + stop.
        let bytes = [0b1010_0110, 0b0100_0010, 0b1100_0000];
        let mut r = BitReader::new(&bytes);
        let got: Vec<i32> = (0..5).map(|_| r.se().expect("se")).collect();
        assert_eq!(got, vec![0, 1, -1, 2, -2]);
    }

    #[test]
    fn running_off_the_end_is_an_error_not_a_panic() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.bits(8).expect("8 bits"), 0xff);
        assert!(r.flag().is_err());
        // 32+ leading zeros.
        let mut r = BitReader::new(&[0, 0, 0, 0, 0x80]);
        assert!(r.ue().is_err());
        let mut r = BitReader::new(&[0x00]);
        assert!(r.ue().is_err());
    }

    #[test]
    fn largest_ue_fits() {
        // 31 zeros, 1, 31 ones → 2^32 − 2.
        let mut w = Vec::new();
        let bits: Vec<bool> = std::iter::repeat_n(false, 31)
            .chain(std::iter::once(true))
            .chain(std::iter::repeat_n(true, 31))
            .collect();
        for chunk in bits.chunks(8) {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                b |= (bit as u8) << (7 - i);
            }
            w.push(b);
        }
        let mut r = BitReader::new(&w);
        assert_eq!(r.ue().expect("ue"), u32::MAX - 1);
    }

    #[test]
    fn ranges_are_enforced() {
        // ue = 3 (00100).
        let mut r = BitReader::new(&[0b0010_0000]);
        assert!(r.ue_max(2, "x").is_err());
        let mut r = BitReader::new(&[0b0010_0000]);
        assert_eq!(r.ue_max(3, "x").expect("in range"), 3);
        // se = −1 (011).
        let mut r = BitReader::new(&[0b0110_0000]);
        assert!(r.se_range(0, 5, "y").is_err());
    }

    #[test]
    fn more_rbsp_data_stops_at_the_stop_bit() {
        // Data bits 1,0 then the stop bit and padding, then trailing zero bytes.
        let bytes = [0b1010_0000, 0x00, 0x00];
        let mut r = BitReader::new(&bytes);
        assert!(r.more_rbsp_data());
        r.flag().expect("bit");
        assert!(r.more_rbsp_data());
        r.flag().expect("bit");
        assert!(!r.more_rbsp_data());
        assert!(!BitReader::new(&[0, 0]).more_rbsp_data());
    }

    #[test]
    fn unescape_removes_only_emulation_bytes() {
        assert_eq!(
            unescape(&[0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3]),
            vec![0x67, 0, 0, 1, 0, 0, 0, 0]
        );
        // A 03 not after two zeros is data.
        assert_eq!(unescape(&[0, 3, 0, 0, 2]), vec![0, 3, 0, 0, 2]);
        // 00 00 03 03: the first 03 goes, the second is data.
        assert_eq!(unescape(&[0, 0, 3, 3]), vec![0, 0, 3]);
    }
}
