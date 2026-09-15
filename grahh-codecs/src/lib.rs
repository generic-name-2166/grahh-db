//! Columnar compression for GRAHH-DB.
//!
//! The public surface is deliberately small and opaque: a [`Codec<T>`] trait
//! (per column type `T`) and the singular [`CoreCodec`] that implements it.
//! Callers do `encode`/`decode` against `&dyn Codec<T>` (or `CoreCodec`) and
//! never choose or inspect the underlying format.
//!
//! Internally, [`CoreCodec`] probes a set of *formats* — delta/FOR,
//! bit-packing, RLE, plain — with a counting sink, picks the smallest, and
//! writes a self-describing 1-byte tag so decoding needs no prior knowledge.
//!
//! The *format* set is open (add a format + register its tag in
//! [`crate::format`]); the *public* API is closed. Callers cannot register
//! user codecs.

mod bitpack;
mod codec;
mod for_;
mod format;
mod plain;
mod rle;
mod sink;

pub use codec::{Codec, CoreCodec};

/// Error conditions surfaced by any codec while encoding/decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The byte buffer is shorter than its header/layout declares.
    Truncated,
    /// The header is internally inconsistent (impossible width/count/tag).
    Corrupt,
    /// The recovered value overflows the expected type during decoding.
    Overflow,
}

/// Read the format tag out of an encoded buffer's leading byte and map it to a
/// stable human-readable name (for diagnostics, tests, and debugging).
pub fn format_name(bytes: &[u8]) -> Option<&'static str> {
    match *bytes.first()? {
        format::TAG_FOR => Some("for"),
        format::TAG_BITPACK => Some("bitpack"),
        format::TAG_RLE => Some("rle"),
        format::TAG_PLAIN => Some("plain"),
        _ => None,
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// Drive any per-type codec behind `&dyn Codec<u64>`.
    fn roundtrip_via_dyn(codec: &dyn Codec<u64>, values: &[u64]) {
        let mut buf = Vec::new();
        codec.encode(values, &mut buf).unwrap();
        let mut out = Vec::new();
        codec.decode(&buf, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn core_codec_roundtrips_behind_the_trait() {
        let c: CoreCodec = CoreCodec;
        let columns: &[&[u64]] = &[
            &[],
            &[0],
            &[0, 0, 0, 0],
            &[1, 2, 3],
            &[0, u64::MAX],
            &[3, 3, 5, 5, 5, 2, 2],
            &[1, 2, 2, 3, 0, 0, 0, 4],
        ];
        for column in columns {
            roundtrip_via_dyn(&c, column);
        }
    }

    #[test]
    fn unknown_tag_is_corrupt() {
        let mut out = Vec::new();
        // tag 0xFF not registered
        assert_eq!(CoreCodec.decode(&[0xff, 0, 0, 0, 0], &mut out), Err(CodecError::Corrupt));
    }
}
