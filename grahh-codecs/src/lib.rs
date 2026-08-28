//! Columnar compression codecs for GRAHH-DB.
//!
//! This crate defines the *thin, uniform codec seam* that every storage layer
//! below depends on. A [`Codec`] turns a homogeneous column of values into a
//! self-describing byte buffer and back. Because a codec's output carries
//! everything its decoder needs (element count, layout parameters), callers
//! treat codecs as interchangeable black boxes — the engine depends on the
//! trait, never on a codec's internals.
//!
//! Codecs in this crate:
//!
//! | codec | struct | behaviour | notes |
//! |-------|--------|-----------|-------|
//! | plain | [`Plain`] | raw passthrough | the no-op; also the size fallback |
//! | bitpack | [`Bitpack`] | fixed-width bit-packing | great for bounded integers |
//! | rle | [`Rle`] | run-length encoding | great for long repeated runs |
//!
//! Adding a future codec (frame-of-reference, dictionary, front-coding,
//! enum, ...) is just a new module with a single `impl Codec<T>` — no changes
//! to the existing codecs or to the layers that consume them.

mod bitpack;
mod plain;
mod rle;

pub use crate::{bitpack::Bitpack, plain::Plain, rle::Rle};

/// Error conditions surfaced by any [`Codec`] while decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The byte buffer is shorter than its header/layout declares.
    Truncated,
    /// The header is internally inconsistent (impossible width/count).
    Corrupt,
    /// The recovered value overflows the expected type during decoding.
    Overflow,
}

/// A columnar compression codec: a homogeneous column `&[T]` <-> a
/// self-describing byte buffer.
///
/// The buffer written by [`encode`](Codec::encode) carries everything
/// [`decode`](Codec::decode) needs to reconstruct the column, so callers
/// never reach into a codec's internals. This is what lets new codecs slot in
/// without touching the engine.
pub trait Codec<T> {
    /// Encode `values`, appending the self-describing buffer to `out`.
    ///
    /// `out` is appended to (not reset) so multiple columns can be written
    /// into one shared buffer during chunk assembly. If `out` already has
    /// data, the encoder's bytes are pushed after it.
    fn encode(&self, values: &[T], out: &mut Vec<u8>) -> Result<(), CodecError>;

    /// Decode the entire `bytes` buffer back into `out`.
    ///
    /// Reads exactly one self-describing codec buffer from `bytes`. Any
    /// trailing bytes (e.g. a later column in a shared chunk buffer) are
    /// ignored.
    fn decode(&self, bytes: &[u8], out: &mut Vec<T>) -> Result<(), CodecError>;

    /// Stable name used in chunk headers and for debug/selection.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod codec_contract_tests {
    use super::*;

    /// A column whose every value is representable in `w` bits round-trips
    /// through every codec in the crate behind a single `&dyn Codec<u64>`.
    fn roundtrip_via_dyn(codec: &dyn Codec<u64>, values: &[u64]) {
        let mut buf = Vec::new();
        codec.encode(values, &mut buf).unwrap();
        let mut out = Vec::new();
        codec.decode(&buf, &mut out).unwrap();
        assert_eq!(out, values, "codec '{}' broke roundtrip", codec.name());
    }

    #[test]
    fn all_codecs_are_swappable_behind_the_trait() {
        let codecs: Vec<&dyn Codec<u64>> = vec![&Plain, &Bitpack, &Rle];
        let columns = [
            &[][..],
            &[0][..],
            &[0, 0, 0, 0],
            &[1, 2, 3],
            &[0, u64::MAX],
            &[1, 2, 2, 3, 0, 0, 0, 4],
        ];
        for codec in &codecs {
            for column in &columns {
                roundtrip_via_dyn(*codec, column);
            }
        }
    }
}
