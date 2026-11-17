//! Embedded database connection over the LSMT engine.
//!
//! `Database` acts like a SQLite/DuckDB-style connection: open a directory,
//! mutate/query directly, and let the engine manage durability (WAL + flush).
//! There is no separate save/load image step.
//!
//! Identity (user key ↔ dense internal id), adjacency edges, and properties
//! all live in the same LSMT key space.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use grahh_lsm::{Engine, EngineConfig, Key, LsmError};

use crate::{
    adjacency::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId},
    identity::NodeId,
    properties::{PropertyKindId, PropertyStore, PropertyType, PropertyValue},
};

/// Reserved LSM kind for label index rows: key `(user_key, LABEL)`, value `(dense_id, 0)`.
const LABEL_KIND_ID: u32 = u32::MAX;
/// Reserved LSM kind for inverse declarations: key `(edge_kind, INVERSE)`, value `(1, 0)`.
const INVERSE_KIND_ID: u32 = u32::MAX - 1;
/// Reserved kind range for internal metadata.
const RESERVED_KIND_FLOOR: u32 = u32::MAX - 1;

fn check_kind(kind: u32) -> Result<(), LsmError> {
    if kind >= RESERVED_KIND_FLOOR {
        Err(LsmError::Corrupt(format!(
            "kind {kind} collides with reserved metadata kinds"
        )))
    } else {
        Ok(())
    }
}

/// A live database connection backed by an LSMT engine.
///
/// Mutations are written to the engine (WAL-durable per engine semantics).
/// Call [`Database::flush`] to force a tablet flush/checkpoint.
pub struct Database {
    engine: Engine,
    /// user key -> dense internal id
    key_to_id: BTreeMap<u64, NodeId>,
    /// dense internal id -> user key
    id_to_key: Vec<u64>,
    next_id: NodeId,
    out_edges: BTreeMap<u32, Vec<(NodeId, NodeId)>>,
    in_edges: BTreeMap<u32, Vec<(NodeId, NodeId)>>,
    inverses: BTreeSet<u32>,
}

impl Database {
    /// Open (or create) a database at `path` with default engine settings.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LsmError> {
        Self::open_with_config(path, EngineConfig::default())
    }

    /// Open (or create) a database with explicit engine settings.
    pub fn open_with_config(
        path: impl AsRef<Path>,
        config: EngineConfig,
    ) -> Result<Self, LsmError> {
        let engine = Engine::open_with_config(path, config)?;
        let mut db = Self {
            engine,
            key_to_id: BTreeMap::new(),
            id_to_key: Vec::new(),
            next_id: 0,
            out_edges: BTreeMap::new(),
            in_edges: BTreeMap::new(),
            inverses: BTreeSet::new(),
        };
        db.load_state()?;
        Ok(db)
    }

    fn load_state(&mut self) -> Result<(), LsmError> {
        // Labels
        let mut label_rows = self.engine.scan_kind(LABEL_KIND_ID)?;
        label_rows.sort_by_key(|(_, value)| value.target_id);
        for (expected, (user_key, value)) in label_rows.iter().enumerate() {
            let expected = expected as u64;
            if value.target_id != expected {
                return Err(LsmError::Corrupt(format!(
                    "label ids are not dense: expected {expected}, found {}",
                    value.target_id
                )));
            }
            if self.key_to_id.insert(*user_key, value.target_id).is_some() {
                return Err(LsmError::Corrupt(
                    "label index contains a duplicate user key".into(),
                ));
            }
            self.id_to_key.push(*user_key);
        }
        self.next_id = self.id_to_key.len() as u64;

        // Inverses
        for (kind, _) in self.engine.scan_kind(INVERSE_KIND_ID)? {
            let kind = u32::try_from(kind)
                .map_err(|_| LsmError::Corrupt("inverse kind id does not fit u32".into()))?;
            check_kind(kind)?;
            self.inverses.insert(kind);
        }

        // Edges (adjacency kinds have 16-byte [target_id, edge_id] rows).
        // Kinds with non-adjacency/raw values (e.g. properties) are left in the
        // engine and accessed via raw/property APIs.
        for kind in self.engine.kinds()? {
            if kind == LABEL_KIND_ID || kind == INVERSE_KIND_ID || kind >= RESERVED_KIND_FLOOR {
                continue;
            }
            let rows = match self.engine.scan_kind(kind) {
                Ok(rows) => rows,
                Err(LsmError::InvalidValueLength { .. }) => continue,
                Err(e) => return Err(e),
            };
            let mut out = Vec::with_capacity(rows.len());
            for (source, value) in rows {
                out.push((source, value.target_id));
            }
            if self.inverses.contains(&kind) {
                let mut rev: Vec<(NodeId, NodeId)> = out.iter().map(|&(s, t)| (t, s)).collect();
                rev.sort_unstable();
                self.in_edges.insert(kind, rev);
            }
            self.out_edges.insert(kind, out);
        }

        // Validate adjacency references
        let max_ref = self
            .out_edges
            .values()
            .flat_map(|v| v.iter())
            .map(|&(s, t)| s.max(t))
            .max()
            .unwrap_or(0);
        if !self.id_to_key.is_empty() && max_ref >= self.next_id {
            return Err(LsmError::Corrupt(
                "adjacency references ids the label index does not know".into(),
            ));
        }
        if self.id_to_key.is_empty() && !self.out_edges.is_empty() {
            return Err(LsmError::Corrupt(
                "adjacency exists without label index".into(),
            ));
        }

        Ok(())
    }

    /// Ensure a node exists for `user_key`, returning its dense internal id.
    /// Creates and persists the mapping if unseen.
    pub fn ensure_node(&mut self, user_key: u64) -> Result<NodeId, LsmError> {
        if let Some(&id) = self.key_to_id.get(&user_key) {
            return Ok(id);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.key_to_id.insert(user_key, id);
        self.id_to_key.push(user_key);
        self.engine
            .add(Key::new(user_key, LABEL_KIND_ID), id, 0)?;
        Ok(id)
    }

    /// Alias for [`Self::ensure_node`].
    pub fn node(&mut self, user_key: u64) -> Result<NodeId, LsmError> {
        self.ensure_node(user_key)
    }

    /// Number of known nodes.
    pub fn node_count(&self) -> usize {
        self.id_to_key.len()
    }

    /// Resolve a user key to its internal id.
    pub fn lookup(&self, user_key: u64) -> Option<NodeId> {
        self.key_to_id.get(&user_key).copied()
    }

    /// Resolve an internal id to its user key.
    pub fn key_of(&self, node: NodeId) -> Option<u64> {
        self.id_to_key.get(node as usize).copied()
    }

    /// Declare that `kind` has an incoming/reverse index.
    pub fn set_inverse(&mut self, kind: EdgeKindId) -> Result<(), LsmError> {
        check_kind(kind.0)?;
        if self.inverses.insert(kind.0) {
            self.engine
                .add(Key::new(u64::from(kind.0), INVERSE_KIND_ID), 1, 0)?;
            if let Some(out) = self.out_edges.get(&kind.0) {
                let mut rev: Vec<(NodeId, NodeId)> =
                    out.iter().map(|&(s, t)| (t, s)).collect();
                rev.sort_unstable();
                self.in_edges.insert(kind.0, rev);
            } else {
                self.in_edges.entry(kind.0).or_default();
            }
        }
        Ok(())
    }

    pub fn has_inverse(&self, kind: EdgeKindId) -> bool {
        self.inverses.contains(&kind.0)
    }

    /// Add a directed edge between user keys, creating nodes if needed.
    pub fn add_edge(
        &mut self,
        kind: EdgeKindId,
        source_key: u64,
        target_key: u64,
    ) -> Result<(), LsmError> {
        check_kind(kind.0)?;
        let source = self.ensure_node(source_key)?;
        let target = self.ensure_node(target_key)?;
        let edge_id = self.next_edge_id(kind.0, source);
        self.engine.add(Key::new(source, kind.0), target, edge_id)?;

        let out = self.out_edges.entry(kind.0).or_default();
        out.push((source, target));
        if self.inverses.contains(&kind.0) {
            let rev = self.in_edges.entry(kind.0).or_default();
            let at = rev.partition_point(|&row| row < (target, source));
            rev.insert(at, (target, source));
        }
        Ok(())
    }

    fn next_edge_id(&self, kind: u32, source: NodeId) -> u64 {
        self.out_edges
            .get(&kind)
            .map(|edges| edges.iter().filter(|&&(s, _)| s == source).count() as u64)
            .unwrap_or(0)
    }

    /// Expand a node through `kind` in `dir`.
    pub fn expand(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<NodeId> {
        match dir {
            Direction::Outgoing => self
                .out_edges
                .get(&kind.0)
                .map(|edges| {
                    edges
                        .iter()
                        .filter(|&&(s, _)| s == node)
                        .map(|&(_, t)| t)
                        .collect()
                })
                .unwrap_or_default(),
            Direction::Incoming => self
                .in_edges
                .get(&kind.0)
                .map(|edges| {
                    let start = edges.partition_point(|&(t, _)| t < node);
                    let end = edges.partition_point(|&(t, _)| t <= node);
                    edges[start..end].iter().map(|&(_, s)| s).collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Convenience: expand from a user key.
    pub fn expand_key(
        &self,
        user_key: u64,
        kind: EdgeKindId,
        dir: Direction,
    ) -> Option<Vec<NodeId>> {
        let node = self.lookup(user_key)?;
        Some(self.expand(node, kind, dir))
    }

    /// Total outgoing edge rows across all kinds.
    pub fn edge_count(&self) -> usize {
        self.out_edges.values().map(Vec::len).sum()
    }

    /// Set/append a property value (LSMT-backed).
    pub fn set_property(
        &mut self,
        node: NodeId,
        property: PropertyKindId,
        value: &PropertyValue,
        value_type: &PropertyType,
    ) -> Result<(), LsmError> {
        check_kind(property.0)?;
        PropertyStore::add(&mut self.engine, node, property, value, value_type)
    }

    /// Get property values for a node.
    pub fn get_property(
        &self,
        node: NodeId,
        property: PropertyKindId,
        value_type: &PropertyType,
    ) -> Result<Vec<PropertyValue>, LsmError> {
        PropertyStore::get(&self.engine, node, property, value_type)
    }

    /// Get a single property value.
    pub fn get_property_one(
        &self,
        node: NodeId,
        property: PropertyKindId,
        value_type: &PropertyType,
    ) -> Result<Option<PropertyValue>, LsmError> {
        PropertyStore::get_one(&self.engine, node, property, value_type)
    }

    /// Build a compressed adjacency snapshot for read-optimized access.
    pub fn adjacency_snapshot(&self) -> AdjacencyStore {
        let mut b = AdjacencyStoreBuilder::new();
        for &kind in &self.inverses {
            b.set_inverse(EdgeKindId(kind));
        }
        for (&kind, edges) in &self.out_edges {
            for &(s, t) in edges {
                b.add_edge(EdgeKindId(kind), s, t);
            }
        }
        b.build()
    }

    /// Flush pending data to tablets.
    pub fn flush(&mut self) -> Result<(), LsmError> {
        self.engine.flush()
    }

    /// Access the underlying engine (advanced use).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable access to the underlying engine (advanced use).
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("node_count", &self.node_count())
            .field("edge_count", &self.edge_count())
            .field("dir", &self.engine.dir())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    const FOLLOWS: EdgeKindId = EdgeKindId(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("grahh-db-test-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_write_expand_reopen() {
        let dir = TempDir::new();
        {
            let mut db = Database::open(&dir.0).unwrap();
            db.set_inverse(FOLLOWS).unwrap();
            db.add_edge(FOLLOWS, 100, 200).unwrap();
            db.add_edge(FOLLOWS, 100, 300).unwrap();
            db.add_edge(FOLLOWS, 200, 300).unwrap();
            db.flush().unwrap();
        }

        let db = Database::open(&dir.0).unwrap();
        assert_eq!(db.node_count(), 3);
        let n100 = db.lookup(100).unwrap();
        let n200 = db.lookup(200).unwrap();
        let n300 = db.lookup(300).unwrap();
        assert_eq!(db.expand(n100, FOLLOWS, Direction::Outgoing), vec![n200, n300]);
        assert_eq!(db.expand(n300, FOLLOWS, Direction::Incoming), vec![n100, n200]);
        assert_eq!(db.lookup(999), None);
    }

    #[test]
    fn incremental_writes_are_visible() {
        let dir = TempDir::new();
        let mut db = Database::open(&dir.0).unwrap();
        db.add_edge(FOLLOWS, 1, 2).unwrap();
        let n1 = db.lookup(1).unwrap();
        let n2 = db.lookup(2).unwrap();
        assert_eq!(db.expand(n1, FOLLOWS, Direction::Outgoing), vec![n2]);
        db.add_edge(FOLLOWS, 1, 3).unwrap();
        let n3 = db.lookup(3).unwrap();
        assert_eq!(db.expand(n1, FOLLOWS, Direction::Outgoing), vec![n2, n3]);
    }

    #[test]
    fn properties_work_through_connection() {
        let dir = TempDir::new();
        let name = PropertyKindId(10_000);
        let ty = PropertyType::String;
        {
            let mut db = Database::open(&dir.0).unwrap();
            let node = db.ensure_node(42).unwrap();
            db.set_property(node, name, &PropertyValue::String("alice".into()), &ty)
                .unwrap();
            db.flush().unwrap();
        }
        let db = Database::open(&dir.0).unwrap();
        let node = db.lookup(42).unwrap();
        assert_eq!(
            db.get_property_one(node, name, &ty).unwrap(),
            Some(PropertyValue::String("alice".into()))
        );
    }

    #[test]
    fn reserved_kinds_are_rejected() {
        let dir = TempDir::new();
        let mut db = Database::open(&dir.0).unwrap();
        assert!(db.add_edge(EdgeKindId(INVERSE_KIND_ID), 1, 2).is_err());
    }

    #[test]
    fn adjacency_snapshot_matches_live_view() {
        let dir = TempDir::new();
        let mut db = Database::open(&dir.0).unwrap();
        db.set_inverse(FOLLOWS).unwrap();
        db.add_edge(FOLLOWS, 1, 2).unwrap(); // keys 1->0, 2->1
        db.add_edge(FOLLOWS, 1, 3).unwrap(); // key 3->2
        let snap = db.adjacency_snapshot();
        let n1 = db.lookup(1).unwrap();
        let n2 = db.lookup(2).unwrap();
        let n3 = db.lookup(3).unwrap();
        assert_eq!(snap.neighbors(n1, FOLLOWS, Direction::Outgoing), vec![n2, n3]);
    }
}
