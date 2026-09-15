//! Frame-of-reference (FOR) / delta + bit-packing format.
//!
//! Ideal for **sorted** integer columns (e.g. neighbor ids). Instead of
//! storing each value, it stores the first value as a reference then
//! bit-packs the deltas `values[i] - values[i-1]`, which are small for
//! monotonic input. This is the workhorse format for adjacency lists.
//!
//! Payload (after the tag byte):
//!
//! ```text
//! [ count : u32 LE ][ width : u8 ][ first : u64 LE ][ gaps : bit-packed, count-1 entries of width bits ]
//! ```
//!
//! `width == 0` (all deltas zero, i.e. every value equal) stores no gap bytes.

use crate::{bitpack::{pack, unpack, unpacked_size}, sink::ByteSink, CodecError};

fn required_gap_width(max_gap: u64) -> u32 {
    if max_gap == 0 {
        0
    } else {
        64 - max_gap.leading_zeros()
    }
}

/// Encode `values` into `sink` as FOR (payload only). Requires non-decreasing
/// input; deltas underflow for decreasing input.
pub(crate) fn encode_u64(values: &[u64], sink: &mut impl ByteSink) {
    assert!(!values.is_empty(), "FOR needs a reference value for an empty column");

    let first: u64 = values[0];
    let mut gaps: Vec<u64> = Vec::with_capacity(values.len() - 1);
    for i in 1..values.len() {
        gaps.push(values[i] - values[i - 1]);
    }
    let width: u32 = required_gap_width(gaps.iter().max().map_or(0, |v: &u64| *v));

    sink.write_u32(values.len() as u32);
    sink.write_u8(width as u8);
    sink.write_u64(first);
    pack(&gaps, width, sink);
}

/// Decode a FOR payload into `out`.
pub(crate) fn decode_u64(
    bytes: &[u8],
    out: &mut Vec<u64>,
) -> Result<(), CodecError> {
    // header: count u32, width u8, first u64, then packed gaps
    const HEADER_BYTES: usize = size_of::<u32>() + size_of::<u8>() + size_of::<u64>();
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let width: u32 = bytes[4] as u32;
    let first = u64::from_le_bytes([
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
    ]);
    if width > 64 {
        return Err(CodecError::Corrupt);
    }

    let gap_count = count.saturating_sub(1);
    if bytes.len() < HEADER_BYTES + unpacked_size(gap_count, width) {
        return Err(CodecError::Truncated);
    }

    out.reserve(count);
    out.push(first);
    if gap_count == 0 {
        return Ok(());
    }
    let mut gaps = Vec::with_capacity(gap_count);
    unpack(&bytes[HEADER_BYTES..], width, gap_count, &mut gaps);
    let mut running = first;
    for g in gaps {
        running = running.checked_add(g).ok_or(CodecError::Overflow)?;
        out.push(running);
    }
    Ok(())
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
    fn roundtrip_monotonic_columns() {
        roundtrip(&[5]);
        roundtrip(&[0, 0, 0]);
        roundtrip(&[1, 2, 3, 4]);
        roundtrip(&[10, 20, 30, 100, 101, 1000]);
        roundtrip(&[0, u64::MAX]);
        roundtrip(&[123, 123, 456]);
    }

    #[test]
    fn repeated_values_use_zero_width() {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[7, 7, 7, 7], &mut sink);
        // count=4, width=0, first=7, no gap bytes -> 13-byte payload
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), 4);
        assert_eq!(buf[4], 0);
        assert_eq!(buf.len(), 13);
    }

    #[test]
    fn width_tracks_distribution() {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[10, 10, 11, 15], &mut sink); // gaps 0,1,4 -> 3 bits
        assert_eq!(buf[4], 3);
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let mut out = Vec::new();
        assert_eq!(
            decode_u64(&[0, 0, 0, 0, 0], &mut out),
            Err(CodecError::Truncated)
        );
    }
}