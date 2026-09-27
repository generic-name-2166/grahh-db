//! Portable, self-describing byte format for the adjacency store.
//!
//! This module is the persistence layer's wire format: the single place that
//! decides how a [`ChunkedCsr`](crate::ChunkedCsr) or an
//! [`AdjacencyStore`](crate::AdjacencyStore) becomes bytes and back. It is
//! deliberately **not** struct-punning and **not** a derive-driven binary
//! codec: every field is an explicit, fixed-width, little-endian integer, so a
//! file written on one machine reads correctly on any other, and stays
//! readable by a future version of this code.
//!
//! The rules:
//! - **Fixed-width little-endian everywhere.** Counts, lengths and offsets are
//!   `u64` on the wire — never `usize`, whose width is a platform detail.
//! - **Length-prefixed regions.** Variable-size payloads (compressed chunks,
//!   nested CSRs) carry their byte length, so a reader never guesses where a
//!   region ends and an unknown region can be skipped wholesale.
//! - **Self-describing regions.** Every region starts with [`MAGIC`] plus a
//!   version byte, and every compressed chunk carries its codec's own format
//!   tag, so decoding needs no out-of-band knowledge.
//! - **Chunk payloads are copied verbatim** — persisting never recompresses.
//!   The file layout mirrors the in-memory layout: "on disk == in RAM".
//!
//! The stores own the actual layout ([`ChunkedCsr::serialize`] and friends);
//! this module only provides the primitives they are built from.

use std::{error::Error, fmt};

use grahh_codecs::CodecError;

/// Magic bytes introducing every persisted region.
pub(crate) const MAGIC: [u8; 4] = *b"GHDB";

pub(crate) const VERSION: u32 = 0;

/// Number of bytes in a region header ([`MAGIC`] + [`VERSION`]).
pub(crate) const HEADER_LEN: usize = MAGIC.len() + std::mem::size_of::<u32>();

/// Anything that can go wrong turning bytes back into a store.
///
/// This is fundamentally a *read* error type: serializing a well-formed store
/// only appends bytes and cannot fail, so `serialize` reuses this type purely
/// for signature symmetry with `deserialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceError {
    /// The buffer ended before a field or region was complete.
    UnexpectedEof {
        /// Bytes the reader still needed.
        needed: usize,
        /// Bytes actually left in the buffer.
        remaining: usize,
    },
    /// The leading bytes were not the [`MAGIC`] marker.
    BadMagic {
        /// The four bytes actually found.
        found: [u8; 4],
    },
    /// The region declares a layout version this build does not know.
    ///
    /// The version is *data* — it says which layout the bytes are in. Nothing
    /// is promised about compatibility before 1.0, so anything but the current
    /// value is refused rather than guessed at.
    UnsupportedVersion {
        /// The version actually found.
        found: u32,
    },
    /// A structural invariant of the format was violated, i.e. the buffer is
    /// corrupt (or hand-edited) rather than merely truncated.
    Corrupt {
        /// Human-readable description of the broken invariant.
        reason: &'static str,
    },
    /// A chunk payload does not decode under the codec tag it carries.
    Chunk {
        /// Index of the offending chunk.
        index: usize,
        /// Why the codec rejected it.
        source: CodecError,
    },
    /// A chunk payload decoded to a different number of ids than it declares.
    ChunkLenMismatch {
        /// Index of the offending chunk.
        index: usize,
        /// Number of ids the directory claims.
        declared: usize,
        /// Number of ids the payload actually produced.
        decoded: usize,
    },
    /// A chunk's decoded targets are out of order within a node's slice, which
    /// no store this engine builds can produce.
    ChunkUnsorted {
        /// Index of the offending chunk.
        index: usize,
    },
    /// The registry lists the same edge kind twice.
    DuplicateKind {
        /// The repeated kind id.
        kind: u32,
    },
    /// A nested region failed to load; `context` says which one it was.
    Nested {
        /// Where it failed, e.g. `"kind 1 in-CSR"`.
        context: String,
        /// The underlying failure.
        source: Box<PersistenceError>,
    },
    /// Bytes remained after the region the header described.
    TrailingBytes {
        /// Number of leftover bytes.
        extra: usize,
    },
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { needed, remaining } => write!(
                f,
                "unexpected end of input: needed {needed} byte(s), {remaining} remaining"
            ),
            Self::BadMagic { found } => {
                write!(f, "bad magic: expected {MAGIC:?}, found {found:?}")
            }
            Self::UnsupportedVersion { found } => write!(
                f,
                "unsupported adjacency format version {found} (this build writes {VERSION})"
            ),
            Self::Corrupt { reason } => write!(f, "corrupt adjacency region: {reason}"),
            Self::Chunk { index, source } => {
                write!(f, "chunk {index}: payload does not decode ({source:?})")
            }
            Self::ChunkLenMismatch {
                index,
                declared,
                decoded,
            } => write!(
                f,
                "chunk {index}: declares {declared} id(s) but decoded {decoded}"
            ),
            Self::ChunkUnsorted { index } => {
                write!(f, "chunk {index}: targets are out of order within a node")
            }
            Self::DuplicateKind { kind } => write!(f, "edge kind {kind} is listed twice"),
            Self::Nested { context, source } => write!(f, "{context}: {source}"),
            Self::TrailingBytes { extra } => {
                write!(f, "{extra} trailing byte(s) after the described region")
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Nested { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Appends the format's primitives to a byte buffer.
///
/// A thin, allocation-free shim over the sink `Vec<u8>`, kept separate from the
/// stores so the byte layout lives in exactly one place.
pub(crate) struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    /// Wrap a buffer to append to (existing contents are kept).
    pub(crate) fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }

    /// Write the region header ([`MAGIC`] + [`VERSION`]).
    pub(crate) fn header(&mut self) {
        self.out.extend_from_slice(&MAGIC);
        self.u32(VERSION);
    }

    /// Write a single byte.
    pub(crate) fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    /// Write a little-endian `u32` (used for opaque ids such as edge kinds).
    pub(crate) fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian `u64`. All on-wire counts, lengths and offsets go
    /// through here so nothing platform-dependent ever reaches the format.
    pub(crate) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Write raw bytes with no length prefix, for regions whose size the reader
    /// already knows from a preceding field.
    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.out.extend_from_slice(value);
    }

    /// Write a `u64` length-prefixed byte region.
    pub(crate) fn len_prefixed(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.bytes(value);
    }

    /// Reserve a `u64` length prefix and return where to backpatch it.
    ///
    /// Lets a nested region be length-prefixed without buffering it separately:
    /// write a placeholder, serialize in place, then fill the length in with
    /// [`Writer::patch_len`].
    pub(crate) fn reserve_len(&mut self) -> usize {
        let at: usize = self.position();
        self.u64(0);
        at
    }

    /// Fill in a prefix previously reserved by [`Writer::reserve_len`].
    pub(crate) fn patch_len(&mut self, at: usize, len: usize) {
        let bytes: [u8; 8] = (len as u64).to_le_bytes();
        self.out[at..at + bytes.len()].copy_from_slice(&bytes);
    }

    /// Current length of the sink, i.e. where the next byte will land.
    pub(crate) fn position(&self) -> usize {
        self.out.len()
    }

    /// The underlying sink, so a nested store can serialize straight into it.
    pub(crate) fn sink(&mut self) -> &mut Vec<u8> {
        self.out
    }
}

/// Reads the format's primitives back out of a byte slice.
///
/// Tracks a cursor and never panics on bad input: every method is bounds
/// checked and reports [`PersistenceError::UnexpectedEof`] instead.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Start reading at the beginning of a region.
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Consume exactly `n` bytes or fail with the shortfall.
    fn take(&mut self, n: usize) -> Result<&'a [u8], PersistenceError> {
        let remaining: usize = self.remaining();
        if n > remaining {
            return Err(PersistenceError::UnexpectedEof {
                needed: n,
                remaining,
            });
        }
        let start: usize = self.pos;
        self.pos += n;
        Ok(&self.buf[start..self.pos])
    }

    /// Read and validate the region header, leaving the cursor after it.
    pub(crate) fn header(&mut self) -> Result<(), PersistenceError> {
        let raw: &[u8] = self.take(HEADER_LEN)?;
        let (raw_magic, raw_version) = raw.split_at(MAGIC.len());
        let mut magic: [u8; 4] = [0; 4];
        magic.copy_from_slice(raw_magic);
        if magic != MAGIC {
            return Err(PersistenceError::BadMagic { found: magic });
        }
        let mut version: [u8; 4] = [0; 4];
        version.copy_from_slice(raw_version);
        let found: u32 = u32::from_le_bytes(version);
        if found != VERSION {
            return Err(PersistenceError::UnsupportedVersion { found });
        }
        Ok(())
    }

    /// Read a single byte.
    pub(crate) fn u8(&mut self) -> Result<u8, PersistenceError> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32, PersistenceError> {
        let raw: &[u8] = self.take(4)?;
        let mut bytes: [u8; 4] = [0; 4];
        bytes.copy_from_slice(raw);
        Ok(u32::from_le_bytes(bytes))
    }

    /// Read a little-endian `u64`.
    pub(crate) fn u64(&mut self) -> Result<u64, PersistenceError> {
        let raw: &[u8] = self.take(8)?;
        let mut bytes: [u8; 8] = [0; 8];
        bytes.copy_from_slice(raw);
        Ok(u64::from_le_bytes(bytes))
    }

    /// Read a `u64` length as a platform `usize`.
    ///
    /// On a 32-bit target a stored length may not be addressable; that is a
    /// corrupt/incompatible file rather than a truncation, so it is reported as
    /// [`PersistenceError::Corrupt`].
    pub(crate) fn len_u64(&mut self) -> Result<usize, PersistenceError> {
        let raw: u64 = self.u64()?;
        usize::try_from(raw).map_err(|_| PersistenceError::Corrupt {
            reason: "length does not fit in usize on this platform",
        })
    }

    /// Read a `u64` length-prefixed byte region.
    pub(crate) fn len_prefixed(&mut self) -> Result<&'a [u8], PersistenceError> {
        let len: usize = self.len_u64()?;
        self.take(len)
    }

    /// Assert the whole region was consumed.
    ///
    /// Catches buffers longer than the region they claim to be, which would
    /// otherwise let a corrupt file masquerade as a valid one.
    pub(crate) fn finish(&self) -> Result<(), PersistenceError> {
        let extra: usize = self.remaining();
        if extra == 0 {
            Ok(())
        } else {
            Err(PersistenceError::TrailingBytes { extra })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = Writer::new(&mut buf);
        w.header();
        w.u8(0xAB);
        w.u32(0x0102_0304);
        w.u64(0xDEAD_BEEF_1234_5678);
        w.len_prefixed(b"payload");
        w.bytes(b"raw");
        drop(w);

        // Header + u8 (1) + u32 (4) + u64 (8) + (8 + 7) + 3.
        assert_eq!(buf.len(), HEADER_LEN + 1 + 4 + 8 + 8 + 7 + 3);
        // Written little-endian and self-describing, not in host order.
        assert_eq!(&buf[..4], b"GHDB");
        assert_eq!(&buf[4..HEADER_LEN], &VERSION.to_le_bytes());
        assert_eq!(&buf[HEADER_LEN..HEADER_LEN + 1], &[0xAB]);

        let mut r = Reader::new(&buf);
        r.header().unwrap();
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u32().unwrap(), 0x0102_0304);
        assert_eq!(r.u64().unwrap(), 0xDEAD_BEEF_1234_5678);
        assert_eq!(r.len_prefixed().unwrap(), b"payload");
        assert_eq!(r.take(3).unwrap(), b"raw");
        r.finish().unwrap();
    }

    #[test]
    fn reserved_length_is_patched_in_place() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = Writer::new(&mut buf);
        w.header();
        let at: usize = w.reserve_len();
        let start: usize = w.position();
        w.bytes(b"nested region");
        let len: usize = w.position() - start;
        w.patch_len(at, len);
        drop(w);

        let mut r = Reader::new(&buf);
        r.header().unwrap();
        assert_eq!(r.len_prefixed().unwrap(), b"nested region");
        r.finish().unwrap();
    }

    #[test]
    fn bad_magic_is_reported() {
        let buf: [u8; 8] = *b"XXXX\x01\x00\x00\x00";
        assert_eq!(
            Reader::new(&buf).header(),
            Err(PersistenceError::BadMagic { found: *b"XXXX" })
        );
    }

    #[test]
    fn unsupported_version_is_reported() {
        let buf: [u8; 8] = *b"GHDB\x7F\x00\x00\x00";
        assert_eq!(
            Reader::new(&buf).header(),
            Err(PersistenceError::UnsupportedVersion { found: 0x7F })
        );
    }

    #[test]
    fn truncation_and_trailing_bytes_are_reported() {
        let short: [u8; 3] = *b"GHD";
        assert_eq!(
            Reader::new(&short).header(),
            Err(PersistenceError::UnexpectedEof {
                needed: HEADER_LEN,
                remaining: 3,
            })
        );

        let mut buf: Vec<u8> = Vec::new();
        let mut w = Writer::new(&mut buf);
        w.header();
        w.u64(7);
        drop(w);

        // Exactly the one `u64` written: reads cleanly, nothing left over.
        let mut r = Reader::new(&buf);
        r.header().unwrap();
        assert_eq!(r.u64().unwrap(), 7);
        r.finish().unwrap();

        // One byte short of the `u64`: a field-level truncation.
        let mut r = Reader::new(&buf[..buf.len() - 1]);
        r.header().unwrap();
        assert_eq!(
            r.u64(),
            Err(PersistenceError::UnexpectedEof {
                needed: 8,
                remaining: 7,
            })
        );

        // Extra bytes after the region the fields described.
        let mut extra: Vec<u8> = buf.clone();
        extra.extend_from_slice(&[0, 0]);
        let mut r = Reader::new(&extra);
        r.header().unwrap();
        r.u64().unwrap();
        assert_eq!(
            r.finish(),
            Err(PersistenceError::TrailingBytes { extra: 2 })
        );
    }

    #[test]
    fn errors_are_usable_as_std_errors() {
        fn as_error() -> Result<(), Box<dyn Error>> {
            Reader::new(b"GHDB\x02\x00\x00\x00").header()?;
            Ok(())
        }
        let text: String = as_error().unwrap_err().to_string();
        assert!(
            text.contains("unsupported adjacency format version 2"),
            "{text}"
        );
    }
}
