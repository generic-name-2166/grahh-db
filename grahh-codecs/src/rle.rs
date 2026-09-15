//! Run-length encoding (RLE) format.
//!
//! Collapses each maximal run of identical `u64` values into a single
//! `(value, length)` pair — ideal for columns with long repeated runs (a
//! boolean, an enum/status column, an edge-kind column, ...). Payload (after
//! the tag byte):
//!
//! ```text
//! [ count : u32 LE ][ n_runs : u32 LE ][ (value: u64 LE)(run_len: u32 LE) × n_runs ]
//! ```
//!
//! `count` mirrors the original column length so the decoder can validate and
//! size allocations; `n_runs` is the number of maximal runs.
//!
//! The *worst* case (every value distinct) is larger than the plain format, so
//! RLE only wins when it actually reduces size — which the codec picker decides.

use crate::{sink::ByteSink, CodecError};

const HEADER_BYTES: usize = size_of::<u32>() * 2;
const RUN_BYTES: usize = size_of::<u64>() + size_of::<u32>();

fn write_count(count: usize, sink: &mut impl ByteSink) {
    sink.write_u32(count as u32);
}

fn write_run(value: u64, run_len: usize, sink: &mut impl ByteSink) {
    sink.write_u64(value);
    sink.write_u32(run_len as u32);
}

/// Encode `values` into `sink` (payload only).
pub(crate) fn encode_u64(values: &[u64], sink: &mut impl ByteSink) {
    let runs = encode_runs(values);
    write_count(values.len(), sink);
    write_count(runs.len(), sink);
    for (value, len) in runs {
        write_run(value, len, sink);
    }
}

/// Decode an RLE payload into `out`.
pub(crate) fn decode_u64(
    bytes: &[u8],
    out: &mut Vec<u64>,
) -> Result<(), CodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let expected_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let n_runs = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

    let run_bytes = n_runs.checked_mul(RUN_BYTES).ok_or(CodecError::Corrupt)?;
    if bytes.len() < HEADER_BYTES + run_bytes {
        return Err(CodecError::Truncated);
    }

    out.reserve(expected_count);
    let mut actual: usize = 0;
    let mut off: usize = HEADER_BYTES;
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
        off += RUN_BYTES;
    }
    if actual != expected_count {
        return Err(CodecError::Corrupt);
    }
    Ok(())
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
    use crate::sink::VecSink;
    use super::*;

    fn roundtrip(values: &[u64]) {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(values, &mut sink);
        let mut out = Vec::new();
        decode_u64(&buf, &mut out).unwrap();
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
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[1, 1, 2, 2, 2], &mut sink);
        // count=5, n_runs=2
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), 5);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 2);
    }

    #[test]
    fn corrupt_zero_length_run_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&2_u32.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        buf.extend_from_slice(&5_u64.to_le_bytes());
        buf.extend_from_slice(&0_u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(decode_u64(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn run_sum_must_match_count() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&5_u32.to_le_bytes());
        buf.extend_from_slice(&2_u32.to_le_bytes());
        buf.extend_from_slice(&7_u64.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        buf.extend_from_slice(&8_u64.to_le_bytes());
        buf.extend_from_slice(&1_u32.to_le_bytes());
        let mut out = Vec::new();
        assert_eq!(decode_u64(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[4, 4], &mut sink);
        buf.push(0xAA); // trailing garbage
        let mut out = Vec::new();
        assert_eq!(decode_u64(&buf, &mut out), Ok(()));
        assert_eq!(out, vec![4, 4]);
    }
}
