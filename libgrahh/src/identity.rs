//! External key to dense internal node id mapping.
//!
//! This is the in-memory identity layer used by the database. Durable storage
//! of label mappings is handled by the LSM engine (reserved metadata kinds),
//! not by this module's own byte image.

use std::collections::BTreeMap;

/// Dense internal node id used by adjacency/property storage.
pub type NodeId = u64;

/// Builds a stable key -> dense id mapping.
#[derive(Debug, Default)]
pub struct LabelIndexBuilder {
    ids: BTreeMap<u64, NodeId>,
    next_id: NodeId,
}

impl LabelIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id for `key`, assigning the next dense id if unseen.
    pub fn add(&mut self, key: u64) -> NodeId {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert(key, id);
        id
    }

    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    pub fn build(self) -> LabelIndex {
        let count = self.ids.len();
        let mut keys = Vec::with_capacity(count);
        let mut ids = Vec::with_capacity(count);
        let mut by_id = vec![0_u64; count];

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

/// Immutable bidirectional key <-> dense id index.
#[derive(Debug, Default)]
pub struct LabelIndex {
    keys: Box<[u64]>,
    ids: Box<[NodeId]>,
    by_id: Box<[u64]>,
}

impl LabelIndex {
    pub fn node_count(&self) -> usize {
        self.keys.len()
    }

    pub fn lookup(&self, key: u64) -> Option<NodeId> {
        self.keys.binary_search(&key).ok().map(|at| self.ids[at])
    }

    pub fn key_of(&self, node: NodeId) -> Option<u64> {
        let at = usize::try_from(node).ok()?;
        self.by_id.get(at).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, NodeId)> + '_ {
        self.keys.iter().copied().zip(self.ids.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_dense_ids_in_first_seen_order() {
        let mut b = LabelIndexBuilder::new();
        assert_eq!(b.add(70), 0);
        assert_eq!(b.add(10), 1);
        assert_eq!(b.add(70), 0);
        let index = b.build();
        assert_eq!(index.lookup(70), Some(0));
        assert_eq!(index.lookup(10), Some(1));
        assert_eq!(index.key_of(0), Some(70));
    }

    #[test]
    fn empty_index_is_safe() {
        let index = LabelIndexBuilder::new().build();
        assert_eq!(index.node_count(), 0);
        assert_eq!(index.lookup(1), None);
        assert_eq!(index.key_of(0), None);
    }
}
