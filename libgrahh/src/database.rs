//! The database: identity plus topology, persisted through the LSM layer.
//!
//! The query structures ([`LabelIndex`] and [`AdjacencyStore`]) stay in memory:
//! they are what make `lookup -> expand -> key_of` an index-free pointer chase.
//! The LSM engine is the durable representation. [`Database::save`] exports the
//! in-memory graph into an [`Engine`]; [`Database::load`] rebuilds it from the
//! engine, with the same validation the old single-image loader performed.

use std::collections::BTreeMap;

use grahh_adjacency::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId};
use grahh_index::{LabelIndex, LabelIndexBuilder, NodeId};
use grahh_lsm::{Engine, Key, LsmError};

/// Reserved LSM kind holding the label index: `key = (user_key, LABEL)` and
/// `value = (dense_id, 0)`.
const LABEL_KIND_ID: u32 = u32::MAX;
/// Reserved LSM kind recording which edge kinds declared an inverse:
/// `key = (kind_id, INVERSE)` and `value = (1, 0)`.
const INVERSE_KIND_ID: u32 = u32::MAX - 1;

/// An immutable graph: user keys resolved to dense internal ids, topology per
/// edge kind, persisted through an LSM key-value store.
#[derive(Debug)]
pub struct Database {
    labels: LabelIndex,
    adjacency: AdjacencyStore,
}

impl Database {
    /// Number of nodes (keys) in the database.
    pub fn node_count(&self) -> usize {
        self.labels.node_count()
    }

    /// Total edges across all kinds and directions.
    pub fn edge_count(&self) -> usize {
        self.adjacency.edge_count()
    }

    /// Resolve a user key to its internal node id.
    pub fn lookup(&self, key: u64) -> Option<NodeId> {
        self.labels.lookup(key)
    }

    /// Resolve an internal node id back to its user key.
    pub fn key_of(&self, node: NodeId) -> Option<u64> {
        self.labels.key_of(node)
    }

    /// Expand a node through `kind` in direction `dir`, as internal ids.
    pub fn expand(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<NodeId> {
        self.adjacency.neighbors(node, kind, dir)
    }

    /// Export this database into an LSM engine and flush it.
    ///
    /// The engine is expected to be a fresh database directory. Writing into a
    /// populated engine is allowed but additive: keys that are not part of this
    /// graph are left untouched.
    ///
    /// # Errors
    /// [`LsmError`] if the engine cannot be written, or if a user edge kind
    /// collides with one of the reserved metadata kinds.
    pub fn save(&self, engine: &mut Engine) -> Result<(), LsmError> {
        // Identity first: user key -> dense id.
        for (user_key, id) in self.labels.iter() {
            engine.add(Key::new(user_key, LABEL_KIND_ID), id, 0)?;
        }

        for kind in self.adjacency.kinds() {
            if kind.0 >= INVERSE_KIND_ID {
                return Err(LsmError::Corrupt(format!(
                    "edge kind {} collides with a reserved metadata kind",
                    kind.0
                )));
            }

            if self.adjacency.has_inverse(kind) {
                engine.add(Key::new(u64::from(kind.0), INVERSE_KIND_ID), 1, 0)?;
            }

            // Parallel edges are distinct rows; the CSR already keeps them in
            // (source, target) order, so the generated edge ids are stable.
            let mut next_edge_id: BTreeMap<NodeId, u64> = BTreeMap::new();
            for (source, target) in self.adjacency.out_edges(kind) {
                let edge_id = next_edge_id.entry(source).or_insert(0);
                engine.add(Key::new(source, kind.0), target, *edge_id)?;
                *edge_id += 1;
            }
        }

        engine.flush()
    }

    /// Rebuild a database exported by [`Database::save`].
    ///
    /// The label index is rebuilt first, then inverse declarations, then the
    /// topology. Every id referenced by an edge — source or target — must be
    /// covered by the label index, so a dangling reference fails the load
    /// rather than surfacing later as a missing key.
    ///
    /// # Errors
    /// [`LsmError`] if the engine cannot be read, its label ids are not dense,
    /// or its topology references ids the label index does not know.
    pub fn load(engine: &Engine) -> Result<Self, LsmError> {
        // Rebuild dense ids in id order, which is exactly the order
        // `LabelIndexBuilder` would have assigned on the original build.
        let mut label_rows = engine.scan_kind(LABEL_KIND_ID)?;
        label_rows.sort_by_key(|(_, value)| value.target_id);
        let mut labels = LabelIndexBuilder::new();
        for (expected, (user_key, value)) in label_rows.iter().enumerate() {
            let expected = expected as u64;
            if value.target_id != expected {
                return Err(LsmError::Corrupt(format!(
                    "label ids are not dense: expected {expected}, found {}",
                    value.target_id
                )));
            }
            let assigned = labels.add(*user_key);
            if assigned != value.target_id {
                return Err(LsmError::Corrupt(
                    "label index contains a duplicate user key".into(),
                ));
            }
        }

        let mut adjacency = AdjacencyStoreBuilder::new();
        for (kind, _) in engine.scan_kind(INVERSE_KIND_ID)? {
            let kind = u32::try_from(kind)
                .map_err(|_| LsmError::Corrupt("inverse kind id does not fit u32".into()))?;
            if kind >= INVERSE_KIND_ID {
                return Err(LsmError::Corrupt(
                    "inverse metadata names a reserved kind".into(),
                ));
            }
            adjacency.set_inverse(EdgeKindId(kind));
        }

        for kind in engine.kinds()? {
            if kind == LABEL_KIND_ID || kind == INVERSE_KIND_ID {
                continue;
            }
            if kind >= INVERSE_KIND_ID {
                return Err(LsmError::Corrupt(format!(
                    "engine contains reserved metadata kind {kind}"
                )));
            }
            for (source, value) in engine.scan_kind(kind)? {
                adjacency.add_edge(EdgeKindId(kind), source, value.target_id);
            }
        }

        let labels: LabelIndex = labels.build();
        let adjacency: AdjacencyStore = adjacency.build();

        // Every source and target must resolve in the identity layer.
        if adjacency.id_span() > labels.node_count() {
            return Err(LsmError::Corrupt(
                "adjacency references ids the label index does not know".into(),
            ));
        }

        Ok(Self { labels, adjacency })
    }
}

/// Assemble a database from keys and key-to-key edges.
///
/// Ids are issued on first sight, so declaring edges by key never requires the
/// caller to know internal ids — and the same input sequence always produces the
/// same ids.
#[derive(Debug, Default)]
pub struct DatabaseBuilder {
    labels: LabelIndexBuilder,
    adjacency: AdjacencyStoreBuilder,
}

impl DatabaseBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The internal id for `key`, assigning the next dense id if unseen.
    pub fn node(&mut self, key: u64) -> NodeId {
        self.labels.add(key)
    }

    /// Declare a directed edge between two user keys.
    pub fn edge(&mut self, kind: EdgeKindId, source: u64, target: u64) {
        let source: NodeId = self.node(source);
        let target: NodeId = self.node(target);
        self.adjacency.add_edge(kind, source, target);
    }

    /// Declare that `kind` should also expose an incoming (reverse) adjacency.
    pub fn set_inverse(&mut self, kind: EdgeKindId) {
        self.adjacency.set_inverse(kind);
    }

    /// Freeze into an immutable [`Database`].
    pub fn build(self) -> Database {
        Database {
            labels: self.labels.build(),
            adjacency: self.adjacency.build(),
        }
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
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Keys 100/200/300, with 100 -> {200, 300} and 200 -> 300.
    fn small_graph() -> Database {
        let mut b = DatabaseBuilder::new();
        b.set_inverse(FOLLOWS);
        b.edge(FOLLOWS, 100, 200);
        b.edge(FOLLOWS, 100, 300);
        b.edge(FOLLOWS, 200, 300);
        b.build()
    }

    fn round_trip(db: &Database) -> Database {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        db.save(&mut engine).unwrap();
        drop(engine);
        let engine = Engine::open(&dir.0).unwrap();
        Database::load(&engine).unwrap()
    }

    #[test]
    fn lookup_then_expand_survives_an_lsm_round_trip() {
        let db = round_trip(&small_graph());

        assert_eq!(db.node_count(), 3);
        // Three forward edges plus three reverse ones: `FOLLOWS` declares an
        // inverse, so both directions are stored in the in-memory registry.
        assert_eq!(db.edge_count(), 6);

        let key_of = |node: NodeId| db.key_of(node).expect("ids resolve back to keys");
        let follows = |key: u64, dir: Direction| -> Vec<u64> {
            db.expand(db.lookup(key).expect("known key"), FOLLOWS, dir)
                .into_iter()
                .map(key_of)
                .collect()
        };

        assert_eq!(follows(100, Direction::Outgoing), vec![200, 300]);
        assert_eq!(follows(200, Direction::Outgoing), vec![300]);
        assert_eq!(follows(100, Direction::Incoming), Vec::<u64>::new());
        assert_eq!(follows(300, Direction::Incoming), vec![100, 200]);
        assert_eq!(db.lookup(999), None);
    }

    #[test]
    fn empty_database_round_trips() {
        let db = round_trip(&DatabaseBuilder::new().build());
        assert_eq!(db.node_count(), 0);
        assert_eq!(db.edge_count(), 0);
        assert_eq!(db.lookup(1), None);
    }

    #[test]
    fn inverse_declarations_survive() {
        let db = round_trip(&small_graph());
        assert_eq!(
            db.expand(db.lookup(300).unwrap(), FOLLOWS, Direction::Incoming),
            vec![db.lookup(100).unwrap(), db.lookup(200).unwrap()]
        );
    }

    #[test]
    fn multiple_kinds_and_parallel_edges_survive() {
        const LIKES: EdgeKindId = EdgeKindId(1);
        let mut b = DatabaseBuilder::new();
        b.edge(FOLLOWS, 1, 2);
        b.edge(FOLLOWS, 1, 3);
        b.edge(LIKES, 1, 2);
        b.edge(LIKES, 1, 2); // parallel edge
        let db = round_trip(&b.build());

        assert_eq!(
            db.expand(db.lookup(1).unwrap(), LIKES, Direction::Outgoing),
            vec![db.lookup(2).unwrap(), db.lookup(2).unwrap()]
        );
        assert_eq!(
            db.expand(db.lookup(1).unwrap(), FOLLOWS, Direction::Outgoing),
            vec![db.lookup(2).unwrap(), db.lookup(3).unwrap()]
        );
    }

    #[test]
    fn a_node_with_no_edges_still_round_trips() {
        let mut b = DatabaseBuilder::new();
        b.edge(FOLLOWS, 100, 200);
        b.node(300);
        let db = round_trip(&b.build());

        assert_eq!(db.node_count(), 3);
        assert_eq!(db.lookup(300), Some(2));
        assert_eq!(
            db.expand(2, FOLLOWS, Direction::Outgoing),
            Vec::<NodeId>::new()
        );
        assert_eq!(db.key_of(2), Some(300));
    }

    #[test]
    fn load_rejects_topology_that_outruns_the_label_index() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        engine.add(Key::new(7, LABEL_KIND_ID), 0, 0).unwrap();
        // Source 5 is not covered by the single label row.
        engine.add(Key::new(5, FOLLOWS.0), 0, 0).unwrap();
        engine.flush().unwrap();

        assert!(matches!(Database::load(&engine), Err(LsmError::Corrupt(_))));
    }

    #[test]
    fn save_rejects_reserved_kind_ids() {
        let dir = TempDir::new();
        let mut engine = Engine::open(&dir.0).unwrap();
        let mut b = DatabaseBuilder::new();
        b.edge(EdgeKindId(INVERSE_KIND_ID), 1, 2);
        let db = b.build();
        assert!(matches!(db.save(&mut engine), Err(LsmError::Corrupt(_))));
    }
}
