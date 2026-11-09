//! The label index: user key ↔ dense internal node id.
//!
//! Dense ids are what make the adjacency layer cheap (one array index per hop),
//! and they are unstable by design. This index is the opposite: it is the
//! stable, user-facing handle, and the thing a rewrite has to preserve.

use std::collections::BTreeMap;

use grahh_codecs::{Codec, CoreCodec};
use grahh_persist::{PersistenceError, Reader, Writer};

/// A dense internal node id: an index into the stores, not a stable reference.
pub type NodeId = u64;

/// Collect user keys and hand out dense internal ids.
///
/// Ids are issued in the order keys are first seen, so the same input sequence
/// always produces the same ids — which is what keeps compaction (a renumbering
/// pass) deterministic.
#[derive(Debug, Default)]
pub struct LabelIndexBuilder {
    ids: BTreeMap<u64, NodeId>,
    next_id: NodeId,
}

impl LabelIndexBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The id for `key`, assigning the next dense id if it is unseen.
    pub fn add(&mut self, key: u64) -> NodeId {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id: NodeId = self.next_id;
        self.next_id += 1;
        self.ids.insert(key, id);
        id
    }

    /// Number of distinct keys seen so far.
    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    /// Freeze into a lookup-ready index (keys sorted, reverse array built).
    pub fn build(self) -> LabelIndex {
        let count: usize = self.ids.len();
        let mut keys: Vec<u64> = Vec::with_capacity(count);
        let mut ids: Vec<NodeId> = Vec::with_capacity(count);
        let mut by_id: Vec<u64> = vec![0; count];

        // `BTreeMap` iterates in key order, which is the order `keys` needs.
        for (key, id) in self.ids {
            keys.push(key);
            ids.push(id);
            by_id[id as usize] = key;
        }

        LabelIndex {
            keys: keys.into_boxed_slice(),
            ids: ids.into_boxed_slice(),
            by_id: by_id.into_boxed_slice(),
        }
    }
}

/// User key ↔ dense internal id, both directions.
///
/// Immutable: built by [`LabelIndexBuilder`] or loaded from bytes.
#[derive(Debug)]
pub struct LabelIndex {
    /// Sorted user keys.
    keys: Box<[u64]>,
    /// Internal id per key, aligned with `keys`.
    ids: Box<[NodeId]>,
    /// User key per dense internal id.
    by_id: Box<[u64]>,
}

impl LabelIndex {
    /// Number of keys — the database's node count while there is no node store.
    pub fn node_count(&self) -> usize {
        self.keys.len()
    }

    /// The internal id for `key`.
    pub fn lookup(&self, key: u64) -> Option<NodeId> {
        self.keys.binary_search(&key).ok().map(|at| self.ids[at])
    }

    /// The user key for an internal id.
    pub fn key_of(&self, node: NodeId) -> Option<u64> {
        let at: usize = usize::try_from(node).ok()?;
        self.by_id.get(at).copied()
    }

    /// Iterate `(user key, internal id)` pairs in key order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, NodeId)> + '_ {
        self.keys.iter().copied().zip(self.ids.iter().copied())
    }

    /// Serialize into the portable byte format, appending to `out`.
    ///
    /// Layout: `header`, key count, then two length-prefixed columns of verbatim
    /// codec output — `keys` (sorted) and `ids` (the permutation key → id). The
    /// reverse direction is rebuilt on load, so it costs nothing on the wire.
    ///
    /// # Errors
    /// Never fails for a well-formed index; the `Result` mirrors
    /// [`LabelIndex::deserialize`].
    pub fn serialize(&self, out: &mut Vec<u8>) -> Result<(), PersistenceError> {
        let mut w: Writer<'_> = Writer::new(out);
        w.header();
        w.u64(self.keys.len() as u64);
        write_column(&mut w, &self.keys);
        write_column(&mut w, &self.ids);
        Ok(())
    }

    /// Rebuild an index written by [`LabelIndex::serialize`].
    ///
    /// Validates everything before trusting it: both columns decode to the
    /// declared count, keys are strictly increasing (so they are unique and
    /// sorted), and `ids` is exactly a permutation of `0..count`. The last two
    /// together mean the reverse direction can be built without further checks,
    /// and that a corrupted column cannot pass silently.
    ///
    /// # Errors
    /// [`PersistenceError`] if the buffer is truncated, mislabelled, has
    /// undecodable columns, or violates any of the invariants above.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, PersistenceError> {
        let mut r: Reader<'_> = Reader::new(bytes);
        r.header()?;
        let count: usize = r.len_u64()?;

        // No capacity hints from the file's count: decode into fresh vectors and
        // compare afterwards, so a bogus count cannot drive an allocation.
        let mut keys: Vec<u64> = Vec::new();
        CoreCodec
            .decode(r.len_prefixed()?, &mut keys)
            .map_err(|source| PersistenceError::Column {
                name: "keys",
                source,
            })?;
        let mut ids: Vec<NodeId> = Vec::new();
        CoreCodec
            .decode(r.len_prefixed()?, &mut ids)
            .map_err(|source| PersistenceError::Column {
                name: "ids",
                source,
            })?;

        if keys.len() != count || ids.len() != count {
            return Err(PersistenceError::Corrupt {
                reason: "label columns do not match the declared key count",
            });
        }
        if keys.windows(2).any(|pair| pair[1] <= pair[0]) {
            return Err(PersistenceError::Corrupt {
                reason: "label keys are not strictly increasing",
            });
        }

        let mut by_id: Vec<u64> = vec![0; count];
        let mut seen: Vec<bool> = vec![false; count];
        for (at, &id) in ids.iter().enumerate() {
            let index: usize = usize::try_from(id).map_err(|_| PersistenceError::Corrupt {
                reason: "label id does not fit in usize on this platform",
            })?;
            if index >= count || seen[index] {
                return Err(PersistenceError::Corrupt {
                    reason: "label ids are not a permutation of the key count",
                });
            }
            seen[index] = true;
            by_id[index] = keys[at];
        }

        r.finish()?;

        Ok(Self {
            keys: keys.into_boxed_slice(),
            ids: ids.into_boxed_slice(),
            by_id: by_id.into_boxed_slice(),
        })
    }
}

/// Write one column as a length-prefixed run of verbatim codec output.
///
/// The length is reserved and backpatched, so the codec writes straight into the
/// sink and no intermediate buffer is needed.
fn write_column(w: &mut Writer<'_>, values: &[u64]) {
    let at: usize = w.reserve_len();
    let start: usize = w.position();
    CoreCodec
        .encode(values, w.sink())
        .expect("encoding a u64 column into a Vec cannot fail");
    let len: usize = w.position() - start;
    w.patch_len(at, len);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_from(keys: &[u64]) -> LabelIndex {
        let mut b = LabelIndexBuilder::new();
        for &key in keys {
            b.add(key);
        }
        b.build()
    }

    /// Hand-build a label region with an explicit declared count.
    fn write_count(out: &mut Vec<u8>, count: u64, keys: &[u64], ids: &[u64]) {
        let mut w = Writer::new(out);
        w.header();
        w.u64(count);
        write_column(&mut w, keys);
        write_column(&mut w, ids);
    }

    fn write_index(out: &mut Vec<u8>, keys: &[u64], ids: &[u64]) {
        write_count(out, keys.len() as u64, keys, ids);
    }

    #[test]
    fn assigns_dense_ids_in_first_seen_order() {
        let mut b = LabelIndexBuilder::new();
        assert_eq!(b.add(70), 0);
        assert_eq!(b.add(10), 1);
        assert_eq!(b.add(70), 0, "re-adding a key returns its existing id");
        assert_eq!(b.node_count(), 2);

        let index = b.build();
        assert_eq!(index.node_count(), 2);
        assert_eq!(index.lookup(70), Some(0));
        assert_eq!(index.lookup(10), Some(1));
    }

    #[test]
    fn looks_up_both_directions() {
        let index = build_from(&[5, 99, 7]);
        assert_eq!(index.node_count(), 3);
        assert_eq!(index.lookup(99), Some(1));

        for key in [5, 99, 7] {
            let id: NodeId = index.lookup(key).unwrap();
            assert_eq!(index.key_of(id), Some(key), "key {key}");
        }
        assert_eq!(index.lookup(1234), None);
        assert_eq!(index.key_of(3), None);
    }

    #[test]
    fn round_trips_through_bytes() {
        let index = build_from(&[5, 99, 7, u64::MAX, 0]);
        let mut bytes: Vec<u8> = Vec::new();
        index.serialize(&mut bytes).unwrap();

        let back: LabelIndex = LabelIndex::deserialize(&bytes).unwrap();
        assert_eq!(back.node_count(), index.node_count());
        for key in [5, 99, 7, u64::MAX, 0] {
            assert_eq!(back.lookup(key), index.lookup(key), "key {key}");
        }
        for id in 0..index.node_count() as NodeId {
            assert_eq!(back.key_of(id), index.key_of(id), "id {id}");
        }

        // Re-serializing a loaded index reproduces the same bytes.
        let mut again: Vec<u8> = Vec::new();
        back.serialize(&mut again).unwrap();
        assert_eq!(again, bytes);
    }

    #[test]
    fn empty_index_round_trips() {
        let index = build_from(&[]);
        let mut bytes: Vec<u8> = Vec::new();
        index.serialize(&mut bytes).unwrap();

        let back: LabelIndex = LabelIndex::deserialize(&bytes).unwrap();
        assert_eq!(back.node_count(), 0);
        assert_eq!(back.lookup(1), None);
        assert_eq!(back.key_of(0), None);
    }

    #[test]
    fn deserialize_rejects_truncation_and_trailing_bytes() {
        let index = build_from(&[1, 2, 3]);
        let mut bytes: Vec<u8> = Vec::new();
        index.serialize(&mut bytes).unwrap();

        assert!(matches!(
            LabelIndex::deserialize(&bytes[..bytes.len() - 1]),
            Err(PersistenceError::UnexpectedEof { .. })
        ));

        let mut extra: Vec<u8> = bytes.clone();
        extra.push(0);
        assert!(matches!(
            LabelIndex::deserialize(&extra),
            Err(PersistenceError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn deserialize_rejects_undecodable_column() {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = Writer::new(&mut buf);
        w.header();
        w.u64(1);
        w.len_prefixed(&[0xFF, 0, 0, 0, 0]); // no such codec tag
        w.len_prefixed(&[0xFF, 0, 0, 0, 0]);

        assert!(matches!(
            LabelIndex::deserialize(&buf),
            Err(PersistenceError::Column { name: "keys", .. })
        ));
    }

    #[test]
    fn deserialize_rejects_inconsistent_structure() {
        // Keys not strictly increasing (unsorted, then duplicated).
        for keys in [&[2_u64, 1][..], &[1, 1][..]] {
            let mut buf: Vec<u8> = Vec::new();
            write_index(&mut buf, keys, &[0, 1]);
            assert!(
                matches!(
                    LabelIndex::deserialize(&buf),
                    Err(PersistenceError::Corrupt { .. })
                ),
                "keys {keys:?} should be rejected"
            );
        }

        // Ids duplicated, or out of range for the key count.
        for ids in [&[0_u64, 0][..], &[0, 7][..]] {
            let mut buf: Vec<u8> = Vec::new();
            write_index(&mut buf, &[1, 2], ids);
            assert!(
                matches!(
                    LabelIndex::deserialize(&buf),
                    Err(PersistenceError::Corrupt { .. })
                ),
                "ids {ids:?} should be rejected"
            );
        }

        // Declared count disagreeing with the columns.
        let mut buf: Vec<u8> = Vec::new();
        write_count(&mut buf, 3, &[1, 2], &[0, 1]);
        assert!(matches!(
            LabelIndex::deserialize(&buf),
            Err(PersistenceError::Corrupt { .. })
        ));
    }
}
