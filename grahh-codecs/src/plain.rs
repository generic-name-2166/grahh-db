//! Plain no-op codec.
//!
//! Stores each `u64` verbatim in little-endian. It exists for testing the
//! codec seam with a trivial, always-correct implementation, and doubles as
//! the size fallback: when no fancier codec wins, plain still round-trips.
//!
//! Buffer layout:
//!
//! ```text
//! [ count : u32 LE ][ value : u64 LE ]×count
//! ```

use crate::{Codec, CodecError};

const HEADER_BYTES: usize = size_of::<u32>();

fn write_header(count: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&(count as u32).to_le_bytes());
}

fn read_header(bytes: &[u8]) -> Result<usize, CodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(CodecError::Truncated);
    }
    let count: usize = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    Ok(count)
}

/// Plain no-op codec (raw passthrough).
#[derive(Debug, Default, Clone, Copy)]
pub struct Plain;

impl Codec<u64> for Plain {
    fn encode(&self, values: &[u64], out: &mut Vec<u8>) -> Result<(), CodecError> {
        write_header(values.len(), out);
        out.reserve(values.len() * 8);
        for &v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        Ok(())
    }

    fn decode(&self, bytes: &[u8], out: &mut Vec<u64>) -> Result<(), CodecError> {
        let count: usize = read_header(bytes)?;
        let body: &[u8] = &bytes[4..];
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

    fn name(&self) -> &'static str {
        "plain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Codec;

    fn roundtrip(values: &[u64]) {
        let mut buf = Vec::new();
        Plain.encode(values, &mut buf).unwrap();
        let mut out = Vec::new();
        Plain.decode(&buf, &mut out).unwrap();
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
        assert_eq!(Plain.decode(&buf, &mut out), Err(CodecError::Truncated));
    }
}
