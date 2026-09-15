//! Plain format: raw passthrough of `u64` values in little-endian.
//!
//! This is the always-correct fallback — when no fancier format wins, plain
//! still round-trips. Buffer layout (the payload, *after* its tag byte):
//!
//! ```text
//! [ count : u32 LE ][ value : u64 LE ]×count
//! ```

use crate::{sink::ByteSink, CodecError};

const HEADER_BYTES: usize = size_of::<u32>();

fn write_header(count: usize, sink: &mut dyn ByteSink) {
    sink.write_u32(count as u32);
}

fn read_header(bytes: &[u8]) -> Result<usize, CodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let count: usize = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    Ok(count)
}

/// Encode `values` into `sink` (payload only; the tag is written by the
/// dispatcher).
pub(crate) fn encode_u64(values: &[u64], sink: &mut dyn ByteSink) {
    write_header(values.len(), sink);
    for &v in values {
        sink.write_u64(v);
    }
}

/// Decode a plain payload into `out`.
pub(crate) fn decode_u64(bytes: &[u8], out: &mut Vec<u64>) -> Result<(), CodecError> {
    let count: usize = read_header(bytes)?;
    let body: &[u8] = &bytes[HEADER_BYTES..];
    if body.len() < count * 8 {
        return Err(CodecError::Truncated);
    }
    for c in 0..count {
        let off = c * 8;
        let mut arr: [u8; 8] = [0; 8];
        arr.copy_from_slice(&body[off..off + 8]);
        out.push(u64::from_le_bytes(arr));
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
    fn roundtrip_basic_columns() {
        roundtrip(&[]);
        roundtrip(&[0]);
        roundtrip(&[u64::MAX]);
        roundtrip(&[1, 2, 3, 4]);
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let mut out = Vec::new();
        // Declares count=3 but only supplies 2 values (4 header + 16 body).
        let mut buf = Vec::new();
        buf.extend_from_slice(&3_u32.to_le_bytes());
        buf.extend_from_slice(&1_u64.to_le_bytes());
        buf.extend_from_slice(&2_u64.to_le_bytes());
        assert_eq!(decode_u64(&buf, &mut out), Err(CodecError::Truncated));
    }
}
