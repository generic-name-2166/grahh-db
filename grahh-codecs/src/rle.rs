//! Run-length encoding (RLE) codec.
//!
//! Collapses each maximal run of identical `u64` values into a single
//! `(value, length)` pair — ideal for columns with long repeated runs (a
//! boolean, an enum/status column, an edge-kind column, ...).
//!
//! Buffer layout:
//!
//! ```text
//! [ count : u32 LE ][ n_runs : u32 LE ][ (value: u64 LE)(run_len: u32 LE) × n_runs ]
//! ```
//!
//! `count` mirrors the original column length so the decoder can validate and
//! callers can size allocations; `n_runs` is the number of maximal runs.
//!
//! Note: the *worst* case (every value distinct) is larger than the plain
//! codec, so RLE is only chosen when it actually wins — that selection is a
//! separate concern from this codec's job of lossless round-tripping.

use crate::{Codec, CodecError};

/// Run-length encoding codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct Rle;

impl Codec<u64> for Rle {
    fn encode(&self, values: &[u64], out: &mut Vec<u8>) -> Result<(), CodecError> {
        let runs = encode_runs(values);
        out.extend_from_slice(&(values.len() as u32).to_le_bytes());
        out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
        for (value, len) in runs {
            out.extend_from_slice(&value.to_le_bytes());
            out.extend_from_slice(&(len as u32).to_le_bytes());
        }
        Ok(())
    }

    fn decode(&self, bytes: &[u8], out: &mut Vec<u64>) -> Result<(), CodecError> {
        if bytes.len() < 8 {
            return Err(CodecError::Truncated);
        }
        let expected_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let n_runs = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

        let run_bytes = n_runs.checked_mul(12).ok_or(CodecError::Corrupt)?;
        if bytes.len() < 8 + run_bytes {
            return Err(CodecError::Truncated);
        }

        out.reserve(expected_count);
        let mut actual: usize = 0;
        let mut off: usize = 8;
        for _ in 0..n_runs {
            let mut v: [u8; 8] = [0; 8];
            v.copy_from_slice(&bytes[off..off + 8]);
            let value = u64::from_le_bytes(v);
            let len = u32::from_le_bytes([
                bytes[off + 8],
                bytes[off + 9],
                bytes[off + 10],
                bytes[off + 11],
            ]) as usize;
            if len == 0 {
                return Err(CodecError::Corrupt); // a run length of 0 is impossible
            }
            actual = actual.checked_add(len).ok_or(CodecError::Overflow)?;
            out.extend(std::iter::repeat_n(value, len));
            off += 12;
        }
        if actual != expected_count {
            return Err(CodecError::Corrupt);
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "rle"
    }
}

/// Produce maximal runs of identical values as `(value, run_length)` pairs.
fn encode_runs(values: &[u64]) -> Vec<(u64, usize)> {
    let mut runs: Vec<(u64, usize)> = Vec::new();
    for &v in values {
        match runs.last_mut() {
            Some((last, count)) if *last == v => *count += 1,
            _ => runs.push((v, 1)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Codec;

    fn roundtrip(values: &[u64]) {
        let mut buf = Vec::new();
        Rle.encode(values, &mut buf).unwrap();
        let mut out = Vec::new();
        Rle.decode(&buf, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn roundtrip_run_heavy_columns() {
        roundtrip(&[]);
        roundtrip(&[3, 3, 5, 5, 5, 2, 2, 5, 5]);
        roundtrip(&[7, 7, 7, 7]);
        roundtrip(&[1, 2, 3]);
        roundtrip(&[u64::MAX, u64::MAX, 0]);
        roundtrip(&[9]);
    }

    #[test]
    fn run_counts_detail() {
        let mut buf = Vec::new();
        Rle.encode(&[1, 1, 2, 2, 2], &mut buf).unwrap();
        // count=5, n_runs=2
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), 5);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 2);
    }

    #[test]
    fn corrupt_zero_length_run_is_rejected() {
        // count=2, n_runs=1, one run of length 0 -> Corrupt
        let mut buf = Vec::new();
        buf.extend_from_slice(&2_u32.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        buf.extend_from_slice(&5_u64.to_le_bytes());
        buf.extend_from_slice(&0_u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(Rle.decode(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn run_sum_must_match_count() {
        // says count=5, but runs sum to 2 -> Corrupt
        let mut buf = Vec::new();
        buf.extend_from_slice(&5_u32.to_le_bytes());
        buf.extend_from_slice(&2_u32.to_le_bytes());
        buf.extend_from_slice(&7_u64.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        buf.extend_from_slice(&8_u64.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(Rle.decode(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let mut buf = Vec::new();
        Rle.encode(&[4, 4], &mut buf).unwrap();
        buf.push(0xAA); // trailing garbage
        let mut out = Vec::new();
        assert_eq!(Rle.decode(&buf, &mut out), Ok(()));
        assert_eq!(out, vec![4, 4]);
    }
}
