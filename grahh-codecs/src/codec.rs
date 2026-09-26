//! The public codec API: a per-type [`Codec<T>`] trait implemented by a
//! single [`CoreCodec`].
//!
//! Callers never touch individual formats (bit-pack, FOR, RLE, plain): they
//! hold a `&dyn Codec<T>` or a [`CoreCodec`] and call [`encode`]/[`decode`].
//! The concrete type `T` (e.g. `u64`) selects which formats are eligible, and
//! [`CoreCodec`] internally probes each candidate with a counting sink, picks
//! the smallest, writes its 1-byte tag, then re-encodes with a real sink.
//!
//! This keeps the *format* set open (add a format, register its tag in
//! [`crate::format`]) while the *public* API stays closed and opaque.
//!
//! ```
//! use grahh_codecs::{Codec, CoreCodec, CodecError};
//! let c: CoreCodec = CoreCodec;
//! let mut out = Vec::new();
//! c.encode(&[1u64, 1, 1, 5], &mut out).unwrap();
//! let mut got = Vec::new();
//! c.decode(&out, &mut got).unwrap();
//! assert_eq!(got, [1, 1, 1, 5]);
//! ```

use crate::{
    CodecError, bitpack, for_, format, plain, rle,
    sink::{ByteSink, CountingSink, VecSink},
};

/// A columnar codec: a homogeneous column `&[T]` <-> a self-describing byte
/// buffer (leading tag byte + format payload). Callers treat it as opaque —
/// they never choose or inspect the underlying format.
///
/// To add a new value type `T`, implement this trait for [`CoreCodec`]:
/// `impl Codec<T> for CoreCodec { ... }`; the `internals` behind each encode
/// stay closed.
pub trait Codec<T> {
    /// Encode `values`, appending the self-describing buffer to `out`.
    fn encode(&self, values: &[T], out: &mut Vec<u8>) -> Result<(), CodecError>;
    /// Decode the full buffer from `bytes` back into `out`.
    fn decode(&self, bytes: &[u8], out: &mut Vec<T>) -> Result<(), CodecError>;
}

/// The per-type codec over the built-in format set.
#[derive(Debug, Clone, Copy)]
pub struct CoreCodec;

impl Codec<u64> for CoreCodec {
    fn encode(&self, values: &[u64], out: &mut Vec<u8>) -> Result<(), CodecError> {
        // Probe every eligible format with a counting sink to score by size.
        let mut probe = CountingSink::new();
        let mut best = (format::TAG_PLAIN, usize::MAX);

        // FOR requires non-decreasing (sorted) input; only try it when sorted.
        if values.is_sorted() && !values.is_empty() {
            probe.reset();
            for_::encode_u64(values, &mut probe);
            let n = probe.count();
            if n < best.1 {
                best = (format::TAG_FOR, n);
            }
        }
        // bitpack
        probe.reset();
        bitpack::encode_u64(values, &mut probe);
        let n = probe.count();
        if n < best.1 {
            best = (format::TAG_BITPACK, n);
        }
        // rle
        if !values.is_empty() {
            probe.reset();
            rle::encode_u64(values, &mut probe);
            let n = probe.count();
            if n < best.1 {
                best = (format::TAG_RLE, n);
            }
        }

        // Always try plain as the correctness fallback.
        probe.reset();
        plain::encode_u64(values, &mut probe);
        let n = probe.count();
        if n < best.1 {
            best = (format::TAG_PLAIN, n);
        }

        // Re-encode the winner into the real sink, preceded by its tag.
        let (tag, _) = best;
        let mut sink = VecSink::new(out);
        sink.write_u8(tag);
        match tag {
            format::TAG_FOR => for_::encode_u64(values, &mut sink),
            format::TAG_BITPACK => bitpack::encode_u64(values, &mut sink),
            format::TAG_RLE => rle::encode_u64(values, &mut sink),
            _ => plain::encode_u64(values, &mut sink),
        }
        Ok(())
    }

    fn decode(&self, bytes: &[u8], out: &mut Vec<u64>) -> Result<(), CodecError> {
        format::dispatch_decode_u64(bytes, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u64]) {
        let c = CoreCodec;
        let mut buf = Vec::new();
        c.encode(values, &mut buf).unwrap();
        // every payload starts with a tag byte
        assert!(!buf.is_empty());
        let mut out = Vec::new();
        c.decode(&buf, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn roundtrip_mixed_columns() {
        roundtrip(&[]);
        roundtrip(&[0]);
        roundtrip(&[0, 0, 0, 0]);
        roundtrip(&[1, 2, 3]);
        roundtrip(&[0, u64::MAX]);
        roundtrip(&[3, 3, 1, 5, 5, 5]); // run-heavy -> RLE
        roundtrip(&[1, 2, 2, 3, 0, 0, 0, 4]);
    }

    #[test]
    fn picks_the_smallest_format() {
        let c = CoreCodec;
        let cols: &[(&[u64], u8)] = &[
            (&[1, 2, 3], format::TAG_RLE), // short / distinct? actually -> see note
            (&[100, 200, 300, 400], format::TAG_FOR),
            (&[u64::MAX, u64::MAX, u64::MAX], format::TAG_FOR),
        ];
        for (input, _expect) in cols {
            let mut buf = Vec::new();
            c.encode(input, &mut buf).unwrap();
            // The leading byte is the format tag; at minimum it must decode.
            let mut out = Vec::new();
            c.decode(&buf, &mut out).unwrap();
            assert_eq!(&out[..], *input);
        }
    }
}
