//! Errors raised by the LSM layer.
//!
//! The LSM layer sits on top of [`grahh_persist`] for byte layout, but it also
//! owns file I/O (WAL, manifest, tablets). This type keeps both kinds of
//! failure behind one `Result` so callers do not have to know which layer
//! rejected their bytes.

use std::{error::Error, fmt, io};

use grahh_persist::PersistenceError;

/// Result alias used throughout `grahh-lsm`.
pub type Result<T> = std::result::Result<T, LsmError>;

/// Anything that can go wrong while opening or mutating the LSM store.
#[derive(Debug)]
pub enum LsmError {
    /// The underlying file could not be read or written.
    Io(io::Error),
    /// A nested region (tablet, manifest payload, ...) failed validation.
    Persist(PersistenceError),
    /// A structural invariant was violated; the store is corrupt.
    Corrupt(String),
    /// A file started with the wrong magic marker.
    BadMagic {
        /// The four bytes actually found.
        found: [u8; 4],
    },
    /// A file declares a layout version this build does not understand.
    UnsupportedVersion {
        /// The version actually found.
        found: u32,
    },
    /// A WAL record's CRC did not match its payload.
    WalChecksum {
        /// Byte offset of the record inside the WAL.
        offset: u64,
    },
    /// A WAL record declared a record type this build does not know.
    UnknownRecordKind {
        /// The type byte actually found.
        kind: u8,
    },
    /// A value was not the 16-byte `[target_id][edge_id]` pair this MVP stores.
    InvalidValueLength {
        /// Length actually supplied.
        found: usize,
    },
}

impl fmt::Display for LsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Persist(source) => write!(f, "persistence error: {source}"),
            Self::Corrupt(reason) => write!(f, "corrupt LSM structure: {reason}"),
            Self::BadMagic { found } => write!(f, "bad magic: found {found:?}"),
            Self::UnsupportedVersion { found } => {
                write!(f, "unsupported LSM format version {found}")
            }
            Self::WalChecksum { offset } => {
                write!(f, "WAL record at byte {offset} failed its CRC check")
            }
            Self::UnknownRecordKind { kind } => {
                write!(f, "unknown WAL record kind {kind}")
            }
            Self::InvalidValueLength { found } => write!(
                f,
                "invalid value length {found}: adjacency values must be 16 bytes"
            ),
        }
    }
}

impl Error for LsmError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Persist(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for LsmError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<PersistenceError> for LsmError {
    fn from(source: PersistenceError) -> Self {
        Self::Persist(source)
    }
}
