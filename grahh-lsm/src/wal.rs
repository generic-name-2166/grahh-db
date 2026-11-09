//! The write-ahead log.
//!
//! Records are self-delimiting deltas: `seq`, a record kind, key length, value
//! length, the raw key/value, and a CRC32 over everything before it. The file
//! starts with an 8-byte header (`GWAL` + version).
//!
//! Replay is deliberately forgiving about a *partial* final record: a crash can
//! leave the tail of the last append unwritten, so decoding stops at the first
//! incomplete record and reports how many bytes were valid. A checksum failure
//! on a complete record is corruption and is reported as an error.

use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use crate::{
    error::{LsmError, Result},
    key::Key,
};

/// Magic bytes introducing a WAL file.
pub const WAL_MAGIC: [u8; 4] = *b"GWAL";
/// WAL layout version this build writes.
pub const WAL_VERSION: u32 = 0;
/// Bytes in the WAL file header.
pub const WAL_HEADER_LEN: usize = 8;
/// Fixed bytes before a record's key: seq (8) + kind (1) + keylen (4) + vallen (4).
const RECORD_FRAMING: usize = 17;

/// What a WAL record does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// Append one item to a cell.
    Add = 1,
    /// Remove one item from a cell.
    Remove = 2,
}

impl RecordKind {
    fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            1 => Ok(Self::Add),
            2 => Ok(Self::Remove),
            kind => Err(LsmError::UnknownRecordKind { kind }),
        }
    }
}

/// One decoded WAL record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Global write sequence number.
    pub seq: u64,
    /// Add or remove.
    pub kind: RecordKind,
    /// The affected `(node_id, kind_id)` slot.
    pub key: Key,
    /// Raw item bytes (16 for adjacency values).
    pub value: Vec<u8>,
}

impl Record {
    /// Encode this record, CRC included.
    pub fn encode(&self) -> Vec<u8> {
        let key_bytes = self.key.to_bytes();
        let total = RECORD_FRAMING + key_bytes.len() + self.value.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(self.kind as u8);
        out.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.value.len() as u32).to_le_bytes());
        out.extend_from_slice(&key_bytes);
        out.extend_from_slice(&self.value);
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }
}

/// Decode a run of records.
///
/// Returns the records and the number of bytes consumed. A trailing partial
/// record is left unconsumed (it is the crash tail); a complete record with a
/// bad CRC is an error.
pub fn decode_records(bytes: &[u8]) -> Result<(Vec<Record>, usize)> {
    let mut records = Vec::new();
    let mut pos = 0_usize;

    while pos < bytes.len() {
        let start = pos;
        if bytes.len() - pos < RECORD_FRAMING {
            break;
        }
        let seq = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .expect("framing guarantees 8 bytes"),
        );
        let kind_byte = bytes[pos + 8];
        let key_len = u32::from_le_bytes(
            bytes[pos + 9..pos + 13]
                .try_into()
                .expect("framing guarantees 4 bytes"),
        ) as usize;
        let value_len = u32::from_le_bytes(
            bytes[pos + 13..pos + 17]
                .try_into()
                .expect("framing guarantees 4 bytes"),
        ) as usize;
        let Some(total) = RECORD_FRAMING
            .checked_add(key_len)
            .and_then(|n| n.checked_add(value_len))
            .and_then(|n| n.checked_add(4))
        else {
            return Err(LsmError::Corrupt("WAL record length overflows".into()));
        };
        if bytes.len() - pos < total {
            break;
        }

        let key_start = pos + RECORD_FRAMING;
        let value_start = key_start + key_len;
        let crc_start = value_start + value_len;
        let stored_crc = u32::from_le_bytes(
            bytes[crc_start..crc_start + 4]
                .try_into()
                .expect("length checked"),
        );
        let actual_crc = crc32fast::hash(&bytes[start..crc_start]);
        if stored_crc != actual_crc {
            return Err(LsmError::WalChecksum {
                offset: start as u64,
            });
        }

        let kind = RecordKind::from_byte(kind_byte)?;
        let key = Key::from_bytes(&bytes[key_start..value_start])?;
        records.push(Record {
            seq,
            kind,
            key,
            value: bytes[value_start..crc_start].to_vec(),
        });
        pos += total;
    }

    Ok((records, pos))
}

/// An append-only WAL file.
#[derive(Debug)]
pub struct Wal {
    file: File,
    path: PathBuf,
}

impl Wal {
    /// Open (or create) a WAL at `path`.
    ///
    /// A new file gets its header immediately. An existing file must already
    /// carry a valid header; a truncated or foreign file is rejected rather
    /// than overwritten.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let len = file.metadata()?.len();
        if len == 0 {
            let mut header = Vec::with_capacity(WAL_HEADER_LEN);
            header.extend_from_slice(&WAL_MAGIC);
            header.extend_from_slice(&WAL_VERSION.to_le_bytes());
            file.write_all(&header)?;
            file.sync_data()?;
        } else {
            let bytes = fs::read(&path)?;
            validate_header(&bytes)?;
        }

        file.seek(SeekFrom::End(0))?;
        Ok(Self { file, path })
    }

    /// Append one record and make it durable.
    pub fn append(&mut self, record: &Record) -> Result<()> {
        let bytes = record.encode();
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Replay all complete records.
    ///
    /// The second return value is the number of valid bytes in the file
    /// (header included). If it is smaller than the file length, the engine
    /// should truncate to it before continuing.
    pub fn replay(&self) -> Result<(Vec<Record>, u64)> {
        let bytes = fs::read(&self.path)?;
        validate_header(&bytes)?;
        let (records, consumed) = decode_records(&bytes[WAL_HEADER_LEN..])?;
        let valid_len = (WAL_HEADER_LEN + consumed) as u64;
        Ok((records, valid_len))
    }

    /// Truncate the log back to just its header.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(WAL_HEADER_LEN as u64)?;
        self.file.seek(SeekFrom::Start(WAL_HEADER_LEN as u64))?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Truncate the log to `len` bytes, keeping the header.
    pub fn truncate_to(&mut self, len: u64) -> Result<()> {
        let len = len.max(WAL_HEADER_LEN as u64);
        self.file.set_len(len)?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Current file length in bytes.
    pub fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// `true` if the log has no records after its header.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == WAL_HEADER_LEN as u64)
    }
}

fn validate_header(bytes: &[u8]) -> Result<()> {
    if bytes.len() < WAL_HEADER_LEN {
        return Err(LsmError::Corrupt(
            "WAL is shorter than its file header".into(),
        ));
    }
    let mut magic = [0_u8; 4];
    magic.copy_from_slice(&bytes[..4]);
    if magic != WAL_MAGIC {
        return Err(LsmError::BadMagic { found: magic });
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("8-byte header"));
    if version != WAL_VERSION {
        return Err(LsmError::UnsupportedVersion { found: version });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("grahh-wal-{}-{id}.log", std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn record(seq: u64, kind: RecordKind, node: u64, target: u64) -> Record {
        Record {
            seq,
            kind,
            key: Key::new(node, 1),
            value: crate::key::Value::new(target, 0).to_bytes().to_vec(),
        }
    }

    #[test]
    fn records_round_trip_through_bytes() {
        let records = vec![
            record(1, RecordKind::Add, 10, 20),
            record(2, RecordKind::Remove, 10, 20),
            record(3, RecordKind::Add, 10, 21),
        ];
        let mut bytes = Vec::new();
        for record in &records {
            bytes.extend_from_slice(&record.encode());
        }
        let (decoded, consumed) = decode_records(&bytes).unwrap();
        assert_eq!(decoded, records);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn append_replay_and_truncate() {
        let file = TempFile::new();
        let mut wal = Wal::open(&file.0).unwrap();
        assert!(wal.is_empty().unwrap());

        wal.append(&record(1, RecordKind::Add, 1, 2)).unwrap();
        wal.append(&record(2, RecordKind::Add, 1, 3)).unwrap();
        let (records, valid) = wal.replay().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(valid, wal.len().unwrap());

        wal.truncate().unwrap();
        assert!(wal.is_empty().unwrap());
        let (records, _) = wal.replay().unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn replay_stops_at_a_partial_tail() {
        let file = TempFile::new();
        let mut wal = Wal::open(&file.0).unwrap();
        wal.append(&record(1, RecordKind::Add, 1, 2)).unwrap();
        let good_len = wal.len().unwrap();

        // Simulate a crash mid-append: append a record, then cut it short.
        wal.append(&record(2, RecordKind::Add, 1, 3)).unwrap();
        let full_len = wal.len().unwrap();
        let file_handle = OpenOptions::new().write(true).open(&file.0).unwrap();
        file_handle.set_len(full_len - 3).unwrap();
        drop(file_handle);

        let (records, valid) = wal.replay().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(valid, good_len);
    }

    #[test]
    fn corrupt_checksum_is_an_error() {
        let file = TempFile::new();
        let mut wal = Wal::open(&file.0).unwrap();
        wal.append(&record(1, RecordKind::Add, 1, 2)).unwrap();
        let mut bytes = fs::read(&file.0).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&file.0, &bytes).unwrap();

        assert!(matches!(wal.replay(), Err(LsmError::WalChecksum { .. })));
    }

    #[test]
    fn bad_header_is_rejected() {
        let file = TempFile::new();
        fs::write(&file.0, b"XXXX\x00\x00\x00\x00").unwrap();
        assert!(matches!(Wal::open(&file.0), Err(LsmError::BadMagic { .. })));

        let file = TempFile::new();
        fs::write(&file.0, b"GWAL\x7f\x00\x00\x00").unwrap();
        assert!(matches!(
            Wal::open(&file.0),
            Err(LsmError::UnsupportedVersion { found: 0x7f })
        ));
    }
}
