//! Immutable columnar run files ("tablets").
//!
//! A tablet is partitioned by `kind_id` into compressed chunks. Within a chunk
//! rows are ordered by `(node_id, row order)`. Alongside the mandatory bloom
//! filter, a tablet carries an exact key/length directory: for every key it
//! records how many rows belong to it, including zero. That makes bloom false
//! positives harmless and lets a tablet say authoritatively "this key is
//! present and empty" (replace semantics).
//!
//! ```text
//! header (40 bytes)
//! bloom        : u64 length + bytes
//! key/length   : u64 length + columns
//! chunk dir    : chunk_count x [kind u32 | rows u32 | offset u64 | len u64]
//! chunk data   : per chunk: node-delta codec column + value blob column
//! footer (16)  : header_offset u64 | magic | version
//! ```
//!
//! Values are stored as raw byte blobs, so the same LSMT structure supports
//! fixed adjacency rows (`[target_id][edge_id]`, 16 bytes) and variable-length
//! property rows.

use std::collections::BTreeMap;

use grahh_codecs::{Codec, CoreCodec};

use crate::{
    bloom::BloomFilter,
    error::{LsmError, Result},
    key::{Key, Value},
    memtable::Cell,
    persist::{Reader, Writer},
};

/// Magic bytes introducing a tablet.
pub const TABLET_MAGIC: [u8; 4] = *b"GRTB";
/// Tablet layout version this build writes.
pub const TABLET_VERSION: u32 = 1;
/// Fixed header width.
const HEADER_LEN: usize = 40;
/// Fixed footer width.
const FOOTER_LEN: usize = 16;
/// One chunk-directory entry.
const DIRECTORY_ENTRY_LEN: usize = 24;

/// A decoded chunk, kept compressed between reads.
#[derive(Debug, Clone)]
struct Chunk {
    row_count: u32,
    /// node-delta codec column + blob value column.
    packed: Box<[u8]>,
}

impl Chunk {
    fn decode(&self, kind_id: u32) -> Result<DecodedChunk> {
        let mut r = Reader::new(&self.packed);
        let node_bytes = r.len_prefixed()?;
        let blob_bytes = r.len_prefixed()?;
        r.finish()?;

        let deltas = decode_column(node_bytes, "node_id")?;
        let expected = self.row_count as usize;
        if deltas.len() != expected {
            return Err(LsmError::Corrupt(format!(
                "kind {kind_id} node column has {}, expected {expected}",
                deltas.len()
            )));
        }

        let mut node_ids = Vec::with_capacity(expected);
        let mut previous = 0_u64;
        for delta in deltas {
            previous = previous.checked_add(delta).ok_or_else(|| {
                LsmError::Corrupt(format!("kind {kind_id} node delta overflows u64"))
            })?;
            node_ids.push(previous);
        }

        let values = decode_blob_column(blob_bytes, expected, kind_id)?;
        Ok(DecodedChunk { node_ids, values })
    }
}

struct DecodedChunk {
    node_ids: Vec<u64>,
    values: Vec<Vec<u8>>,
}

/// An immutable columnar run.
#[derive(Debug, Clone)]
pub struct Tablet {
    entry_count: u64,
    min_node: u64,
    max_node: u64,
    bloom: BloomFilter,
    /// Exact key membership plus row count. `0` means the key is present and
    /// empty; absence means the key is not in this tablet at all.
    key_lengths: BTreeMap<Key, u32>,
    chunks: BTreeMap<u32, Chunk>,
}

impl Tablet {
    /// Build a tablet from buffered cells.
    pub fn build(cells: impl IntoIterator<Item = (Key, Cell)>) -> Result<Self> {
        TabletBuilder::from_cells(cells).build()
    }

    /// Total rows across all chunks.
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// Number of distinct keys in the exact directory (empty keys included).
    pub fn key_count(&self) -> usize {
        self.key_lengths.len()
    }

    /// Number of distinct kinds (chunks).
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Lowest node id covered by this tablet.
    pub fn min_node(&self) -> u64 {
        self.min_node
    }

    /// Highest node id covered by this tablet.
    pub fn max_node(&self) -> u64 {
        self.max_node
    }

    /// Iterate the exact key/length directory.
    pub fn keys(&self) -> impl Iterator<Item = (Key, u32)> + '_ {
        self.key_lengths.iter().map(|(&key, &len)| (key, len))
    }

    /// `true` if the bloom filter might contain `key`.
    pub fn may_contain(&self, key: Key) -> bool {
        self.bloom.may_contain(key)
    }

    /// Look up one key, returning raw value blobs.
    ///
    /// `None` means this tablet does not contain the key (including a bloom
    /// false positive). `Some(vec![])` means the key is present and
    /// authoritatively empty.
    pub fn get_raw(&self, key: Key) -> Result<Option<Vec<Vec<u8>>>> {
        if !self.bloom.may_contain(key) {
            return Ok(None);
        }
        let Some(&length) = self.key_lengths.get(&key) else {
            return Ok(None);
        };
        if length == 0 {
            return Ok(Some(Vec::new()));
        }

        let chunk = self.chunks.get(&key.kind_id).ok_or_else(|| {
            LsmError::Corrupt(format!(
                "key {key:?} has rows but kind {} has no chunk",
                key.kind_id
            ))
        })?;
        let decoded = chunk.decode(key.kind_id)?;
        let start = decoded.node_ids.partition_point(|&node| node < key.node_id);
        let end = decoded
            .node_ids
            .partition_point(|&node| node <= key.node_id);
        let found = end - start;
        if found != length as usize {
            return Err(LsmError::Corrupt(format!(
                "key {key:?} declares {length} rows but chunk has {found}"
            )));
        }
        Ok(Some(decoded.values[start..end].to_vec()))
    }

    /// Look up one key as adjacency values (16-byte `[target_id][edge_id]` rows).
    ///
    /// Returns `InvalidValueLength` if rows are not adjacency-shaped.
    pub fn get(&self, key: Key) -> Result<Option<Vec<Value>>> {
        let Some(rows) = self.get_raw(key)? else {
            return Ok(None);
        };
        rows.into_iter()
            .map(|bytes| Value::from_bytes(&bytes))
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }

    /// Every key for `kind_id` with its raw rows, in key order.
    pub fn scan_kind_raw(&self, kind_id: u32) -> Result<Vec<(Key, Vec<Vec<u8>>)>> {
        let decoded = self
            .chunks
            .get(&kind_id)
            .map(|chunk| chunk.decode(kind_id))
            .transpose()?;
        let mut out = Vec::new();
        let mut row = 0_usize;
        for (&key, &length) in self
            .key_lengths
            .iter()
            .filter(|(key, _)| key.kind_id == kind_id)
        {
            let length = length as usize;
            let mut values = Vec::with_capacity(length);
            match &decoded {
                Some(decoded) => {
                    let end = row.checked_add(length).ok_or_else(|| {
                        LsmError::Corrupt(format!("kind {kind_id} row count overflows usize"))
                    })?;
                    if end > decoded.values.len() {
                        return Err(LsmError::Corrupt(format!(
                            "key {key:?} declares {length} rows but kind {kind_id} chunk is too short"
                        )));
                    }
                    values.extend(decoded.values[row..end].iter().cloned());
                    row = end;
                }
                None if length > 0 => {
                    return Err(LsmError::Corrupt(format!(
                        "key {key:?} has rows but kind {kind_id} has no chunk"
                    )));
                }
                None => {}
            }
            out.push((key, values));
        }
        if let Some(decoded) = &decoded
            && row != decoded.values.len()
        {
            return Err(LsmError::Corrupt(format!(
                "kind {kind_id} chunk has {} rows but its keys declare {row}",
                decoded.values.len()
            )));
        }
        Ok(out)
    }

    /// Every key for `kind_id` as adjacency values.
    pub fn scan_kind(&self, kind_id: u32) -> Result<Vec<(Key, Vec<Value>)>> {
        self.scan_kind_raw(kind_id)?
            .into_iter()
            .map(|(key, rows)| {
                let values = rows
                    .into_iter()
                    .map(|bytes| Value::from_bytes(&bytes))
                    .collect::<Result<Vec<_>>>()?;
                Ok((key, values))
            })
            .collect()
    }

    /// Serialize the whole tablet.
    pub fn serialize(&self) -> Vec<u8> {
        let bloom = self.bloom.serialize();
        let key_section = encode_key_index(&self.key_lengths);

        let data_start = HEADER_LEN
            + 8
            + bloom.len()
            + 8
            + key_section.len()
            + self.chunks.len() * DIRECTORY_ENTRY_LEN;
        let mut directory = Vec::with_capacity(self.chunks.len() * DIRECTORY_ENTRY_LEN);
        let mut offset = data_start as u64;
        for (&kind_id, chunk) in &self.chunks {
            directory.extend_from_slice(&kind_id.to_le_bytes());
            directory.extend_from_slice(&chunk.row_count.to_le_bytes());
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(&(chunk.packed.len() as u64).to_le_bytes());
            offset += chunk.packed.len() as u64;
        }

        let mut out = Vec::with_capacity(offset as usize + FOOTER_LEN);
        {
            let mut w = Writer::new(&mut out);
            w.bytes(&TABLET_MAGIC);
            w.u32(TABLET_VERSION);
            w.u64(self.entry_count);
            w.u32(self.chunks.len() as u32);
            w.u64(self.min_node);
            w.u64(self.max_node);
            w.u32(0); // flags
            w.len_prefixed(&bloom);
            w.len_prefixed(&key_section);
            w.bytes(&directory);
            for chunk in self.chunks.values() {
                w.bytes(&chunk.packed);
            }
            w.u64(0);
            w.bytes(&TABLET_MAGIC);
            w.u32(TABLET_VERSION);
        }
        debug_assert_eq!(out.len(), offset as usize + FOOTER_LEN);
        out
    }

    /// Rebuild a tablet written by [`Tablet::serialize`].
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN + FOOTER_LEN {
            return Err(LsmError::Corrupt(
                "tablet is shorter than header + footer".into(),
            ));
        }
        validate_footer(bytes)?;

        let mut r = Reader::new(bytes);
        let magic = r.u32()?;
        if magic != u32::from_le_bytes(TABLET_MAGIC) {
            return Err(LsmError::BadMagic {
                found: magic.to_le_bytes(),
            });
        }
        let version = r.u32()?;
        if version != TABLET_VERSION {
            return Err(LsmError::UnsupportedVersion { found: version });
        }
        let entry_count = r.u64()?;
        let chunk_count = r.u32()? as usize;
        let min_node = r.u64()?;
        let max_node = r.u64()?;
        let flags = r.u32()?;
        if flags != 0 {
            return Err(LsmError::Corrupt("tablet flags are reserved".into()));
        }

        let bloom = BloomFilter::deserialize(r.len_prefixed()?)?;
        let key_section = r.len_prefixed()?;
        let (key_lengths, declared_entries) = decode_key_index(key_section)?;
        if declared_entries != entry_count {
            return Err(LsmError::Corrupt(format!(
                "key directory declares {declared_entries} rows, header declares {entry_count}"
            )));
        }

        if chunk_count > r.remaining() / DIRECTORY_ENTRY_LEN {
            return Err(LsmError::Corrupt(
                "chunk count exceeds the bytes remaining for its directory".into(),
            ));
        }
        let mut entries = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let kind_id = r.u32()?;
            let row_count = r.u32()?;
            let data_offset = r.u64()?;
            let data_length = r.u64()?;
            entries.push((kind_id, row_count, data_offset, data_length));
        }

        let data_start = HEADER_LEN
            + 8
            + bloom.serialize().len()
            + 8
            + key_section.len()
            + chunk_count * DIRECTORY_ENTRY_LEN;
        let mut expected = data_start as u64;
        let mut previous_kind: Option<u32> = None;
        for &(kind_id, row_count, data_offset, data_length) in &entries {
            if previous_kind.is_some_and(|previous| kind_id <= previous) {
                return Err(LsmError::Corrupt(
                    "chunk directory kinds are not strictly increasing".into(),
                ));
            }
            previous_kind = Some(kind_id);
            if row_count == 0 {
                return Err(LsmError::Corrupt(
                    "chunk directory contains an empty chunk".into(),
                ));
            }
            if data_offset != expected {
                return Err(LsmError::Corrupt(
                    "chunk data offsets are not contiguous".into(),
                ));
            }
            let end = data_offset
                .checked_add(data_length)
                .ok_or_else(|| LsmError::Corrupt("chunk data length overflows".into()))?;
            if end > (bytes.len() - FOOTER_LEN) as u64 {
                return Err(LsmError::Corrupt(
                    "chunk data extends past the footer".into(),
                ));
            }
            expected = end;
        }
        if expected != (bytes.len() - FOOTER_LEN) as u64 {
            return Err(LsmError::Corrupt(
                "chunk data does not cover the region before the footer".into(),
            ));
        }

        let mut chunks = BTreeMap::new();
        let mut counts: BTreeMap<Key, u32> = BTreeMap::new();
        let mut total_rows = 0_u64;
        for &(kind_id, row_count, data_offset, data_length) in &entries {
            let start = data_offset as usize;
            let end = start + data_length as usize;
            let packed = bytes[start..end].to_vec().into_boxed_slice();
            let chunk = Chunk { row_count, packed };
            let decoded = chunk.decode(kind_id)?;
            for &node_id in &decoded.node_ids {
                *counts.entry(Key::new(node_id, kind_id)).or_insert(0) += 1;
            }
            total_rows += u64::from(row_count);
            chunks.insert(kind_id, chunk);
        }
        if total_rows != entry_count {
            return Err(LsmError::Corrupt(
                "chunk row counts do not sum to the header entry count".into(),
            ));
        }
        for (&key, &length) in &key_lengths {
            let actual = counts.remove(&key).unwrap_or(0);
            if actual != length {
                return Err(LsmError::Corrupt(format!(
                    "key {key:?} declares {length} rows but chunk data has {actual}"
                )));
            }
        }
        if !counts.is_empty() {
            return Err(LsmError::Corrupt(
                "chunk data contains keys missing from the exact directory".into(),
            ));
        }

        let (expected_min, expected_max) =
            match (key_lengths.keys().next(), key_lengths.keys().next_back()) {
                (Some(first), Some(last)) => (first.node_id, last.node_id),
                _ => (0, 0),
            };
        if min_node != expected_min || max_node != expected_max {
            return Err(LsmError::Corrupt(
                "tablet min/max node does not match its keys".into(),
            ));
        }

        Ok(Self {
            entry_count,
            min_node,
            max_node,
            bloom,
            key_lengths,
            chunks,
        })
    }
}

/// Assemble a tablet from cells.
#[derive(Debug, Default)]
pub struct TabletBuilder {
    cells: BTreeMap<Key, Cell>,
}

impl TabletBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_cells(cells: impl IntoIterator<Item = (Key, Cell)>) -> Self {
        let mut builder = Self::new();
        for (key, cell) in cells {
            builder.insert(key, cell);
        }
        builder
    }

    pub fn insert(&mut self, key: Key, cell: Cell) {
        self.cells.insert(key, cell);
    }

    pub fn build(self) -> Result<Tablet> {
        let mut rows_by_kind: BTreeMap<u32, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
        let mut key_lengths: BTreeMap<Key, u32> = BTreeMap::new();
        let mut entry_count = 0_u64;

        for (key, cell) in &self.cells {
            let length = u32::try_from(cell.len()).map_err(|_| {
                LsmError::Corrupt(format!("cell {key:?} has more than u32::MAX rows"))
            })?;
            key_lengths.insert(*key, length);
            entry_count += u64::from(length);
            for value in cell.iter() {
                rows_by_kind
                    .entry(key.kind_id)
                    .or_default()
                    .push((key.node_id, value.to_vec()));
            }
        }

        let mut chunks = BTreeMap::new();
        for (kind_id, mut rows) in rows_by_kind {
            rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let mut node_ids = Vec::with_capacity(rows.len());
            let mut values = Vec::with_capacity(rows.len());
            for (node, value) in rows {
                node_ids.push(node);
                values.push(value);
            }

            let mut deltas = Vec::with_capacity(node_ids.len());
            let mut previous = 0_u64;
            for &node in &node_ids {
                deltas.push(node.saturating_sub(previous));
                previous = node;
            }

            let mut packed = Vec::new();
            {
                let mut w = Writer::new(&mut packed);
                write_column(&mut w, &deltas)?;
                let blob = encode_blob_column(&values);
                w.len_prefixed(&blob);
            }

            let row_count = u32::try_from(values.len())
                .map_err(|_| LsmError::Corrupt("chunk has more than u32::MAX rows".into()))?;
            chunks.insert(
                kind_id,
                Chunk {
                    row_count,
                    packed: packed.into_boxed_slice(),
                },
            );
        }

        let bloom = BloomFilter::build(key_lengths.keys());
        let (min_node, max_node) = match (key_lengths.keys().next(), key_lengths.keys().next_back()) {
            (Some(first), Some(last)) => (first.node_id, last.node_id),
            _ => (0, 0),
        };

        Ok(Tablet {
            entry_count,
            min_node,
            max_node,
            bloom,
            key_lengths,
            chunks,
        })
    }
}

fn encode_blob_column(values: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = Writer::new(&mut out);
    w.u64(values.len() as u64);
    for value in values {
        w.len_prefixed(value);
    }
    out
}

fn decode_blob_column(bytes: &[u8], expected: usize, kind_id: u32) -> Result<Vec<Vec<u8>>> {
    let mut r = Reader::new(bytes);
    let count = r.len_u64()?;
    if count != expected {
        return Err(LsmError::Corrupt(format!(
            "kind {kind_id} blob column has {count} rows, expected {expected}"
        )));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(r.len_prefixed()?.to_vec());
    }
    r.finish()?;
    Ok(out)
}

fn encode_key_index(keys: &BTreeMap<Key, u32>) -> Vec<u8> {
    let mut node_deltas = Vec::with_capacity(keys.len());
    let mut kind_ids = Vec::with_capacity(keys.len());
    let mut lengths = Vec::with_capacity(keys.len());
    let mut previous = 0_u64;
    for (&key, &length) in keys {
        node_deltas.push(key.node_id.saturating_sub(previous));
        previous = key.node_id;
        kind_ids.push(u64::from(key.kind_id));
        lengths.push(u64::from(length));
    }

    let mut out = Vec::new();
    let mut w = Writer::new(&mut out);
    w.u64(keys.len() as u64);
    write_column(&mut w, &node_deltas).expect("encoding u64 columns cannot fail");
    write_column(&mut w, &kind_ids).expect("encoding u64 columns cannot fail");
    write_column(&mut w, &lengths).expect("encoding u64 columns cannot fail");
    out
}

fn decode_key_index(bytes: &[u8]) -> Result<(BTreeMap<Key, u32>, u64)> {
    let mut r = Reader::new(bytes);
    let count = r.len_u64()?;
    let node_deltas = decode_column(r.len_prefixed()?, "key node_id")?;
    let kind_ids = decode_column(r.len_prefixed()?, "key kind_id")?;
    let lengths = decode_column(r.len_prefixed()?, "key length")?;
    r.finish()?;

    if node_deltas.len() != count || kind_ids.len() != count || lengths.len() != count {
        return Err(LsmError::Corrupt(
            "key directory columns disagree with its declared count".into(),
        ));
    }

    let mut keys = BTreeMap::new();
    let mut previous_node = 0_u64;
    let mut previous_key: Option<Key> = None;
    let mut total = 0_u64;
    for at in 0..count {
        previous_node = previous_node
            .checked_add(node_deltas[at])
            .ok_or_else(|| LsmError::Corrupt("key node delta overflows u64".into()))?;
        let kind_id = u32::try_from(kind_ids[at])
            .map_err(|_| LsmError::Corrupt("key kind id does not fit u32".into()))?;
        let key = Key::new(previous_node, kind_id);
        if previous_key.is_some_and(|previous| key <= previous) {
            return Err(LsmError::Corrupt(
                "key directory is not strictly increasing".into(),
            ));
        }
        previous_key = Some(key);

        let length = u32::try_from(lengths[at])
            .map_err(|_| LsmError::Corrupt("key row count does not fit u32".into()))?;
        total = total
            .checked_add(u64::from(length))
            .ok_or_else(|| LsmError::Corrupt("key row counts overflow u64".into()))?;
        keys.insert(key, length);
    }

    Ok((keys, total))
}

fn write_column(w: &mut Writer<'_>, values: &[u64]) -> Result<()> {
    let at = w.reserve_len();
    let start = w.position();
    CoreCodec
        .encode(values, w.sink())
        .map_err(|source| LsmError::Corrupt(format!("column encode failed: {source:?}")))?;
    let len = w.position() - start;
    w.patch_len(at, len);
    Ok(())
}

fn decode_column(bytes: &[u8], name: &'static str) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    CoreCodec.decode(bytes, &mut out).map_err(|source| {
        LsmError::Corrupt(format!("{name} column does not decode: {source:?}"))
    })?;
    Ok(out)
}

fn validate_footer(bytes: &[u8]) -> Result<()> {
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    let header_offset = u64::from_le_bytes(footer[..8].try_into().expect("footer width"));
    if header_offset != 0 {
        return Err(LsmError::Corrupt(
            "tablet footer points at a nonzero header offset".into(),
        ));
    }
    let mut magic = [0_u8; 4];
    magic.copy_from_slice(&footer[8..12]);
    if magic != TABLET_MAGIC {
        return Err(LsmError::BadMagic { found: magic });
    }
    let version = u32::from_le_bytes(footer[12..].try_into().expect("footer width"));
    if version != TABLET_VERSION {
        return Err(LsmError::UnsupportedVersion { found: version });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(values: &[(u64, u64)]) -> Cell {
        Cell::Many(
            values
                .iter()
                .map(|&(target, edge)| Value::new(target, edge).to_bytes().to_vec())
                .collect(),
        )
    }

    fn sample() -> Tablet {
        let mut b = TabletBuilder::new();
        b.insert(Key::new(10, 0), cell(&[(20, 0), (21, 1)]));
        b.insert(Key::new(10, 1), cell(&[(30, 0)]));
        b.insert(Key::new(11, 0), cell(&[(22, 0)]));
        b.insert(Key::new(12, 1), Cell::empty_many());
        b.build().unwrap()
    }

    #[test]
    fn round_trips_and_answers_lookups() {
        let tablet = sample();
        let bytes = tablet.serialize();
        let back = Tablet::deserialize(&bytes).unwrap();

        assert_eq!(back.entry_count(), 4);
        assert_eq!(back.key_count(), 4);
        assert_eq!(back.chunk_count(), 2);
        assert_eq!(back.min_node(), 10);
        assert_eq!(back.max_node(), 12);

        assert_eq!(
            back.get(Key::new(10, 0)).unwrap(),
            Some(vec![Value::new(20, 0), Value::new(21, 1)])
        );
        assert_eq!(
            back.get(Key::new(10, 1)).unwrap(),
            Some(vec![Value::new(30, 0)])
        );
        assert_eq!(back.get(Key::new(12, 1)).unwrap(), Some(vec![]));
        assert_eq!(back.get(Key::new(99, 0)).unwrap(), None);
    }

    #[test]
    fn scan_kind_keeps_empty_keys_and_skips_other_kinds() {
        let tablet = Tablet::deserialize(&sample().serialize()).unwrap();

        assert_eq!(
            tablet.scan_kind(0).unwrap(),
            vec![
                (Key::new(10, 0), vec![Value::new(20, 0), Value::new(21, 1)]),
                (Key::new(11, 0), vec![Value::new(22, 0)]),
            ]
        );
        assert_eq!(
            tablet.scan_kind(1).unwrap(),
            vec![
                (Key::new(10, 1), vec![Value::new(30, 0)]),
                (Key::new(12, 1), vec![]),
            ]
        );
        assert!(tablet.scan_kind(7).unwrap().is_empty());
    }

    #[test]
    fn empty_tablet_round_trips() {
        let tablet = Tablet::build(std::iter::empty()).unwrap();
        assert_eq!(tablet.entry_count(), 0);
        assert_eq!(tablet.key_count(), 0);
        assert_eq!(tablet.chunk_count(), 0);
        let back = Tablet::deserialize(&tablet.serialize()).unwrap();
        assert_eq!(back.entry_count(), 0);
        assert_eq!(back.get(Key::new(1, 1)).unwrap(), None);
    }

    #[test]
    fn serialization_is_byte_for_byte_stable() {
        let tablet = sample();
        let first = tablet.serialize();
        let back = Tablet::deserialize(&first).unwrap();
        let second = back.serialize();
        assert_eq!(first, second);
    }

    #[test]
    fn parallel_edges_are_preserved_in_order() {
        let mut b = TabletBuilder::new();
        b.insert(Key::new(1, 0), cell(&[(5, 9), (5, 2), (5, 7)]));
        let tablet = b.build().unwrap();
        let rows = tablet.get(Key::new(1, 0)).unwrap().unwrap();
        assert_eq!(
            rows,
            vec![Value::new(5, 2), Value::new(5, 7), Value::new(5, 9)]
        );
    }

    #[test]
    fn corrupt_tablets_are_rejected() {
        let tablet = sample();
        let bytes = tablet.serialize();

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            Tablet::deserialize(&bad_magic),
            Err(LsmError::BadMagic { .. })
        ));

        let mut bad_version = bytes.clone();
        bad_version[4] = 0x7f;
        assert!(matches!(
            Tablet::deserialize(&bad_version),
            Err(LsmError::UnsupportedVersion { .. })
        ));

        assert!(Tablet::deserialize(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Tablet::deserialize(&trailing).is_err());
    }

    #[test]
    fn directory_must_match_rows() {
        let tablet = sample();
        let mut bytes = tablet.serialize();
        let bloom_len =
            u64::from_le_bytes(bytes[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap()) as usize;
        let key_at = HEADER_LEN + 8 + bloom_len;
        let key_len = u64::from_le_bytes(bytes[key_at..key_at + 8].try_into().unwrap()) as usize;
        let directory_at = key_at + 8 + key_len;
        bytes[directory_at + 4..directory_at + 8].copy_from_slice(&0_u32.to_le_bytes());
        assert!(Tablet::deserialize(&bytes).is_err());
    }

    #[test]
    fn keys_with_no_rows_are_authoritative() {
        let mut b = TabletBuilder::new();
        b.insert(Key::new(7, 3), Cell::empty_many());
        let tablet = b.build().unwrap();
        let back = Tablet::deserialize(&tablet.serialize()).unwrap();
        assert_eq!(back.get(Key::new(7, 3)).unwrap(), Some(vec![]));
        assert_eq!(back.entry_count(), 0);
        assert_eq!(back.key_count(), 1);
    }

    #[test]
    fn variable_length_property_rows_round_trip() {
        let mut b = TabletBuilder::new();
        b.insert(Key::new(1, 100), Cell::One(b"hello".to_vec()));
        b.insert(
            Key::new(1, 101),
            Cell::Many(vec![vec![1, 2, 3], b"world".to_vec()]),
        );
        let tablet = Tablet::deserialize(&b.build().unwrap().serialize()).unwrap();
        assert_eq!(
            tablet.get_raw(Key::new(1, 100)).unwrap(),
            Some(vec![b"hello".to_vec()])
        );
        assert_eq!(
            tablet.get_raw(Key::new(1, 101)).unwrap(),
            Some(vec![vec![1, 2, 3], b"world".to_vec()])
        );
    }
}
