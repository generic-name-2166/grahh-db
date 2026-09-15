//! Fixed-width bit-packing format.
//!
//! A column of `u64` values is stored using only as many bits per value as it
//! actually needs (its *required width*), instead of a full 8 bytes.
//! Payload (after the tag byte):
//!
//! ```text
//! [ count : u32 LE ][ width : u8 ][ values : bit-packed, `count` entries of `width` bits ]
//! ```
//!
//! `width == 0` denotes an all-zero column: there are no data bytes because an
//! empty column and an all-zero column both round-trip at width 0.

use crate::{sink::ByteSink, CodecError};

/// Largest width (in *bits*) this module can address (one `u64`).
const MAX_WIDTH: u32 = 64;
/// number of bits in a byte
const BYTE_SIZE: usize = 8;
const HEADER_BYTES: usize = size_of::<u32>() + size_of::<u8>();

/// The parsed bit-packed header: how many values, at what bit-width.
#[derive(Debug, Clone, Copy)]
struct Header {
    count: usize,
    width: u32,
}

fn write_header(header: Header, sink: &mut impl ByteSink) {
    sink.write_u32(header.count as u32);
    sink.write_u8(header.width as u8);
}

fn read_header(bytes: &[u8]) -> Result<Header, CodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let count: usize = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let width: u32 = bytes[4] as u32;
    if width > MAX_WIDTH {
        return Err(CodecError::Corrupt);
    }
    Ok(Header { count, width })
}

/// Number of bytes produced by packing `count` values of `width` bits.
pub(crate) fn unpacked_size(count: usize, width: u32) -> usize {
    if width == 0 {
        0
    } else {
        (count * width as usize).div_ceil(BYTE_SIZE)
    }
}

/// Smallest bit-width able to represent `max_value`.
///
/// A result of `0` (i.e. `max_value == 0`) denotes an all-zero column: it can
/// pack to nothing and decode to zeros.
fn required_width(max_value: u64) -> u32 {
    if max_value == 0 {
        0
    } else {
        MAX_WIDTH - max_value.leading_zeros()
    }
}

/// Pack `values` at `width` bits each, writing to `sink`.
///
/// `width` must be `<= 64`. A `width == 64` column is stored verbatim as
/// little-endian `u64`; a `width == 0` column (all values zero) writes
/// nothing. Inverse is [`unpack`].
pub(crate) fn pack(values: &[u64], width: u32, sink: &mut impl ByteSink) {
    debug_assert!(width <= MAX_WIDTH);
    if width == MAX_WIDTH {
        for &v in values {
            sink.write_u64(v);
        }
        return;
    }
    let mask: u64 = if width == 0 { 0 } else { (1_u64 << width) - 1 };
    let mut bitbuf: u128 = 0;
    let mut bits: u32 = 0;
    for &value in values {
        bitbuf |= ((value & mask) as u128) << bits;
        bits += width;
        while bits >= 8 {
            sink.write_u8((bitbuf & 0xff) as u8);
            bitbuf >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        sink.write_u8((bitbuf & 0xff) as u8);
    }
}

/// Unpack `count` values of `width` bits each from `bytes`, appending to `out`.
///
/// Inverse of [`pack`]. `bytes` must contain at least [`unpacked_size`] bytes.
pub(crate) fn unpack(bytes: &[u8], width: u32, count: usize, out: &mut Vec<u64>) {
    debug_assert!(width <= MAX_WIDTH);
    if width == 64 {
        for c in 0..count {
            let off = c * 8;
            let mut arr: [u8; 8] = [0; 8];
            arr.copy_from_slice(&bytes[off..off + 8]);
            out.push(u64::from_le_bytes(arr));
        }
        return;
    }
    let mask: u64 = if width == 0 { 0 } else { (1_u64 << width) - 1 };
    let mut bitbuf: u128 = 0;
    let mut nbits: u32 = 0;
    let mut byte: usize = 0;
    for _ in 0..count {
        while nbits < width {
            if byte < bytes.len() {
                bitbuf |= (bytes[byte] as u128) << nbits;
                byte += 1;
            }
            nbits += 8;
        }
        out.push((bitbuf & (mask as u128)) as u64);
        bitbuf >>= width;
        nbits -= width;
    }
}

/// Encode `values` into `sink` as a bit-packed payload (excluding the tag,
/// which the caller writes).
pub(crate) fn encode_u64(values: &[u64], sink: &mut impl ByteSink) {
    let max: u64 = values.iter().max().map_or(0, |v: &u64| *v);
    let header = Header {
        count: values.len(),
        width: required_width(max),
    };
    write_header(header, sink);
    pack(values, header.width, sink);
}

/// Decode a bit-packed payload into `out`.
pub(crate) fn decode_u64(
    bytes: &[u8],
    out: &mut Vec<u64>,
) -> Result<(), CodecError> {
    let header = read_header(bytes)?;
    if bytes.len() < HEADER_BYTES + unpacked_size(header.count, header.width) {
        return Err(CodecError::Truncated);
    }
    unpack(&bytes[HEADER_BYTES..], header.width, header.count, out);
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
    fn roundtrip_interesting_columns() {
        roundtrip(&[]);
        roundtrip(&[0]);
        roundtrip(&[0, 0, 0]);
        roundtrip(&[1, 2, 3]);
        roundtrip(&[0, 255, 128]);
        roundtrip(&[u64::MAX]);
        roundtrip(&[1000, 1001, 1002]);
        roundtrip(&[0, 1, 2, 3, 4095]);
    }

    #[test]
    fn header_reflects_width_and_count() {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[1, 2, 3], &mut sink);
        // count=3, width=2 bits (max 3).
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), 3);
        assert_eq!(buf[4], 2);
    }

    #[test]
    fn corrupt_width_is_rejected() {
        let mut buf = Vec::new();
        let mut sink = VecSink::new(&mut buf);
        encode_u64(&[1, 2], &mut sink);
        buf[4] = 200; // impossible width > 64
        let mut out = Vec::new();
        assert_eq!(decode_u64(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let mut out = Vec::new();
        assert_eq!(
            decode_u64(&[1, 2, 3], &mut out),
            Err(CodecError::Truncated)
        );
    }
}
