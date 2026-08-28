//! Fixed-width bit-packing codec.
//!
//! A column of `u64` values is stored using only as many bits per value as it
//! actually needs (its *required width*), instead of a full 8 bytes.
//!
//! Buffer layout:
//!
//! ```text
//! [ count : u32 LE ][ width : u8 ][ values : bit-packed, `count` entries of `width` bits ]
//! ```
//!
//! The value `width == 0` denotes an all-zero column: the three header bytes
//! fit in the same 5-byte header and there are no data bytes because an empty
//! column *and* an all-zero column both round-trip correctly at width 0.

use crate::{Codec, CodecError};

/// Largest width (in *bits*) this module can address.
///
/// size of `u64`
const MAX_WIDTH: u32 = 64;
/// number of bits in a byte
const BYTE_SIZE: usize = 8;
const HEADER_BYTES: usize = size_of::<u32>() + size_of::<u8>();

fn write_header(count: usize, width: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&(count as u32).to_le_bytes());
    out.push(width as u8);
}

fn read_header(bytes: &[u8]) -> Result<(usize, u32), CodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let count: usize = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let width: u32 = bytes[4] as u32;
    if width > MAX_WIDTH {
        return Err(CodecError::Corrupt);
    }
    Ok((count, width))
}

/// Number of bytes produced by packing `count` values of `width` bits.
fn unpacked_size(count: usize, width: u32) -> usize {
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

/// Pack `values` at `width` bits each, appending packed bytes to `out`.
///
/// `width` must be `<= 64`. A `width == 64` column is stored verbatim as
/// little-endian `u64`; a `width == 0` column (all values zero) appends
/// nothing. Inverse is [`unpack`].
fn pack(values: &[u64], width: u32, out: &mut Vec<u8>) {
    debug_assert!(width <= MAX_WIDTH);
    if width == MAX_WIDTH {
        out.reserve(values.len() * 8);
        for &v in values {
            out.extend_from_slice(&v.to_le_bytes());
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
            out.push((bitbuf & 0xff) as u8);
            bitbuf >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push((bitbuf & 0xff) as u8);
    }
}

/// Unpack `count` values of `width` bits each from `bytes`, appending to `out`.
///
/// Inverse of [`pack`]. `bytes` must contain at least [`unpacked_size`] bytes.
fn unpack(bytes: &[u8], width: u32, count: usize, out: &mut Vec<u64>) {
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

/// Fixed-width bit-packing codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bitpack;

impl Codec<u64> for Bitpack {
    fn encode(&self, values: &[u64], out: &mut Vec<u8>) -> Result<(), CodecError> {
        let max: u64 = values.iter().max().map_or(0, |v: &u64| *v);
        let width: u32 = required_width(max);
        write_header(values.len(), width, out);
        pack(values, width, out);
        Ok(())
    }

    fn decode(&self, bytes: &[u8], out: &mut Vec<u64>) -> Result<(), CodecError> {
        let (count, width) = read_header(bytes)?;
        if bytes.len() < unpacked_size(count, width) {
            return Err(CodecError::Truncated);
        }
        unpack(&bytes[5..], width, count, out);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "bitpack"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Codec;

    fn roundtrip(values: &[u64]) {
        let mut buf = Vec::new();
        Bitpack.encode(values, &mut buf).unwrap();
        let mut out = Vec::new();
        Bitpack.decode(&buf, &mut out).unwrap();
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
        Bitpack.encode(&[1, 2, 3], &mut buf).unwrap();
        // count=3, width=2 bits (max 3).
        let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(count, 3);
        assert_eq!(buf[4], 2);
    }

    #[test]
    fn corrupt_width_is_rejected() {
        let mut buf = Vec::new();
        Bitpack.encode(&[1, 2], &mut buf).unwrap();
        buf[4] = 200; // impossible width > 64
        let mut out = Vec::new();
        assert_eq!(Bitpack.decode(&buf, &mut out), Err(CodecError::Corrupt));
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let mut out = Vec::new();
        assert_eq!(
            Bitpack.decode(&[1, 2, 3], &mut out),
            Err(CodecError::Truncated)
        );
    }
}
