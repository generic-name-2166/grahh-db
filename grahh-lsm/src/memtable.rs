//! The in-memory write buffer.
//!
//! A [`Memtable`] is a `BTreeMap<Key, Cell>`: one cell per `(node_id, kind_id)`
//! slot. A cell records the slot's cardinality, so a 1-to-many adjacency list
//! and a single-valued property row share the same map without a type tag on
//! disk.

use std::collections::BTreeMap;

use crate::key::Key;

/// The in-memory value of one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Exactly one value.
    One(Vec<u8>),
    /// Zero or one value.
    ZeroOrOne(Option<Vec<u8>>),
    /// Zero or more values (adjacency lists live here).
    Many(Vec<Vec<u8>>),
}

impl Cell {
    /// An empty 1-to-many cell.
    pub fn empty_many() -> Self {
        Self::Many(Vec::new())
    }

    /// Number of values currently in the cell.
    pub fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::ZeroOrOne(slot) => usize::from(slot.is_some()),
            Self::Many(items) => items.len(),
        }
    }

    /// `true` if the cell holds no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate the values in insertion/row order.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        let slice: &[Vec<u8>] = match self {
            Self::One(value) => std::slice::from_ref(value),
            Self::ZeroOrOne(None) => &[],
            Self::ZeroOrOne(Some(value)) => std::slice::from_ref(value),
            Self::Many(items) => items,
        };
        slice.iter().map(Vec::as_slice)
    }

    /// Apply an `Add` delta.
    ///
    /// `One`/`ZeroOrOne` are set (replaced); `Many` appends, preserving
    /// parallel edges.
    pub fn add(&mut self, value: Vec<u8>) {
        match self {
            Self::One(slot) => *slot = value,
            Self::ZeroOrOne(slot) => *slot = Some(value),
            Self::Many(items) => items.push(value),
        }
    }

    /// Apply a `Remove` delta.
    ///
    /// `Many` removes every matching item; `One`/`ZeroOrOne` become empty.
    pub fn remove(&mut self, value: &[u8]) {
        match self {
            Self::One(_) => *self = Self::ZeroOrOne(None),
            Self::ZeroOrOne(slot) => *slot = None,
            Self::Many(items) => items.retain(|item| item.as_slice() != value),
        }
    }
}

/// Byte-budgeted write buffer.
///
/// The budget is approximate on purpose: it only decides when to flush, never
/// what is stored.
#[derive(Debug, Default)]
pub struct Memtable {
    cells: BTreeMap<Key, Cell>,
    bytes: usize,
}

impl Memtable {
    /// Create an empty memtable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// `true` if no key is buffered.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Approximate bytes held by the buffer (keys + values + per-cell slack).
    pub fn approximate_bytes(&self) -> usize {
        self.bytes
    }

    /// Borrow the cell for `key`.
    pub fn get(&self, key: Key) -> Option<&Cell> {
        self.cells.get(&key)
    }

    /// Insert or replace a whole cell.
    pub fn insert(&mut self, key: Key, cell: Cell) {
        if let Some(old) = self.cells.insert(key, cell) {
            self.bytes = self.bytes.saturating_sub(cell_bytes(&old));
        }
        let cell = self.cells.get(&key).expect("just inserted");
        self.bytes += Key::LEN + cell_bytes(cell);
    }

    /// Ensure `key` has a cell, creating an empty `Many` if absent.
    pub fn entry_many(&mut self, key: Key) -> &mut Cell {
        let existed = self.cells.contains_key(&key);
        let cell = self.cells.entry(key).or_insert_with(Cell::empty_many);
        if !existed {
            self.bytes += Key::LEN + cell_bytes(cell);
        }
        cell
    }

    /// Apply an `Add` delta to `key`, creating a `Many` cell when absent.
    pub fn add(&mut self, key: Key, value: Vec<u8>) {
        let cell = self.entry_many(key);
        let before = cell_bytes(cell);
        cell.add(value);
        let after = cell_bytes(cell);
        self.bytes += after.saturating_sub(before);
    }

    /// Apply a `Remove` delta to `key`, creating an empty `Many` when absent.
    pub fn remove(&mut self, key: Key, value: &[u8]) {
        let cell = self.entry_many(key);
        let before = cell_bytes(cell);
        cell.remove(value);
        let after = cell_bytes(cell);
        let freed = before.saturating_sub(after);
        self.bytes = self.bytes.saturating_sub(freed);
    }

    /// Iterate keys in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Cell)> {
        self.cells.iter()
    }

    /// Consume the buffer, yielding its cells in key order.
    pub fn take(&mut self) -> BTreeMap<Key, Cell> {
        self.bytes = 0;
        std::mem::take(&mut self.cells)
    }
}

/// Approximate heap bytes used by one cell's values.
fn cell_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::One(value) => value.len(),
        Cell::ZeroOrOne(slot) => slot.as_ref().map_or(0, Vec::len),
        Cell::Many(items) => items.iter().map(Vec::len).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(node: u64, kind: u32) -> Key {
        Key::new(node, kind)
    }

    #[test]
    fn add_and_remove_track_multiplicity() {
        let mut mem = Memtable::new();
        mem.add(key(1, 0), vec![1]);
        mem.add(key(1, 0), vec![2]);
        mem.add(key(1, 0), vec![1]);
        assert_eq!(mem.get(key(1, 0)).unwrap().len(), 3);

        mem.remove(key(1, 0), &[1]);
        let values: Vec<&[u8]> = mem.get(key(1, 0)).unwrap().iter().collect();
        assert_eq!(values, vec![&[2_u8][..]]);
    }

    #[test]
    fn removing_everything_keeps_an_authoritative_empty_cell() {
        let mut mem = Memtable::new();
        mem.add(key(5, 2), vec![9]);
        mem.remove(key(5, 2), &[9]);
        let cell = mem.get(key(5, 2)).expect("the key is still present");
        assert!(cell.is_empty());
    }

    #[test]
    fn single_valued_cells_replace_and_clear() {
        let mut cell = Cell::ZeroOrOne(None);
        cell.add(vec![1]);
        assert_eq!(cell.iter().collect::<Vec<_>>(), vec![&[1_u8][..]]);
        cell.add(vec![2]);
        assert_eq!(cell.iter().collect::<Vec<_>>(), vec![&[2_u8][..]]);
        cell.remove(&[2]);
        assert!(cell.is_empty());

        let mut cell = Cell::One(vec![7]);
        cell.add(vec![8]);
        assert_eq!(cell.iter().collect::<Vec<_>>(), vec![&[8_u8][..]]);
        cell.remove(&[8]);
        assert!(cell.is_empty());
    }

    #[test]
    fn take_empties_the_buffer() {
        let mut mem = Memtable::new();
        mem.add(key(1, 0), vec![1]);
        let cells = mem.take();
        assert_eq!(cells.len(), 1);
        assert!(mem.is_empty());
        assert_eq!(mem.approximate_bytes(), 0);
    }
}
