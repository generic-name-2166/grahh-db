//! Wire-format primitives shared by the storage layer.
//!
//! Byte-level primitives for the LSM storage layer. Stores define their layout
//! on top of [`Writer`] and [`Reader`].

use std::{error::Error, fmt};

use grahh_codecs::CodecError;

/// Magic bytes introducing every persisted region.
pub const MAGIC: [u8; 4] = *b"GHDB";

/// Layout version the bytes declare.
pub const VERSION: u32 = 0;

/// Number of bytes in a region header ([`MAGIC`] + [`VERSION`]).
pub const HEADER_LEN: usize = MAGIC.len() + std::mem::size_of::<u32>();

/// Anything that can go wrong turning bytes back into a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceError {
    UnexpectedEof {
        needed: usize,
        remaining: usize,
    },
    BadMagic {
        found: [u8; 4],
    },
    UnsupportedVersion {
        found: u32,
    },
    Corrupt {
        reason: &'static str,
    },
    Chunk {
        index: usize,
        source: CodecError,
    },
    ChunkLenMismatch {
        index: usize,
        declared: usize,
        decoded: usize,
    },
    ChunkUnsorted {
        index: usize,
    },
    Column {
        name: &'static str,
        source: CodecError,
    },
    DuplicateKind {
        kind: u32,
    },
    UnknownRegion {
        tag: u32,
    },
    Nested {
        context: String,
        source: Box<PersistenceError>,
    },
    TrailingBytes {
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
                "unsupported format version {found} (this build writes {VERSION})"
            ),
            Self::Corrupt { reason } => write!(f, "corrupt region: {reason}"),
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
            Self::Column { name, source } => {
                write!(f, "{name} column: payload does not decode ({source:?})")
            }
            Self::DuplicateKind { kind } => write!(f, "edge kind {kind} is listed twice"),
            Self::UnknownRegion { tag } => write!(f, "unknown region tag {tag}"),
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
pub struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }

    pub fn header(&mut self) {
        self.out.extend_from_slice(&MAGIC);
        self.u32(VERSION);
    }

    pub fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    pub fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.out.extend_from_slice(value);
    }

    pub fn len_prefixed(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.bytes(value);
    }

    pub fn reserve_len(&mut self) -> usize {
        let at = self.position();
        self.u64(0);
        at
    }

    pub fn patch(&mut self, at: usize, bytes: &[u8]) {
        self.out[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub fn patch_len(&mut self, at: usize, len: usize) {
        self.patch(at, &(len as u64).to_le_bytes());
    }

    pub fn position(&self) -> usize {
        self.out.len()
    }

    pub fn sink(&mut self) -> &mut Vec<u8> {
        self.out
    }
}

/// Reads the format's primitives back out of a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PersistenceError> {
        let remaining = self.remaining();
        if n > remaining {
            return Err(PersistenceError::UnexpectedEof {
                needed: n,
                remaining,
            });
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.buf[start..self.pos])
    }

    pub fn header(&mut self) -> Result<(), PersistenceError> {
        let raw = self.take(HEADER_LEN)?;
        let (raw_magic, raw_version) = raw.split_at(MAGIC.len());
        let mut magic = [0_u8; 4];
        magic.copy_from_slice(raw_magic);
        if magic != MAGIC {
            return Err(PersistenceError::BadMagic { found: magic });
        }
        let mut version = [0_u8; 4];
        version.copy_from_slice(raw_version);
        let found = u32::from_le_bytes(version);
        if found != VERSION {
            return Err(PersistenceError::UnsupportedVersion { found });
        }
        Ok(())
    }

    pub fn u8(&mut self) -> Result<u8, PersistenceError> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, PersistenceError> {
        let raw = self.take(4)?;
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(raw);
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn u64(&mut self) -> Result<u64, PersistenceError> {
        let raw = self.take(8)?;
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(raw);
        Ok(u64::from_le_bytes(bytes))
    }

    pub fn len_u64(&mut self) -> Result<usize, PersistenceError> {
        let raw = self.u64()?;
        usize::try_from(raw).map_err(|_| PersistenceError::Corrupt {
            reason: "length does not fit in usize on this platform",
        })
    }

    pub fn len_prefixed(&mut self) -> Result<&'a [u8], PersistenceError> {
        let len = self.len_u64()?;
        self.take(len)
    }

    pub fn finish(&self) -> Result<(), PersistenceError> {
        let extra = self.remaining();
        if extra == 0 {
            Ok(())
        } else {
            Err(PersistenceError::TrailingBytes { extra })
        }
    }
}
