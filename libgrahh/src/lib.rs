//! The GRAHH-DB database: identity + topology on top of the LSM layer.
//!
//! This crate is the engine's front door. It keeps the query structures in
//! memory — the [`LabelIndex`] (user key ↔ dense internal id) and the
//! [`AdjacencyStore`] (one compressed CSR per edge kind) — and uses
//! [`grahh_lsm::Engine`] as the durable representation.
//!
//! ```text
//! in memory:  label index + per-kind adjacency CSRs
//! on disk:    LSM tablets keyed by (node_id, kind_id)
//!             value = (target_id, edge_id)
//! ```
//!
//! Build with [`DatabaseBuilder`], [`Database::save`] into an [`Engine`], and
//! [`Database::load`] back. Loading validates the label ids and refuses a
//! topology that references an id the identity layer does not know.
//!
//! ```no_run
//! use libgrahh::{Database, DatabaseBuilder, Direction, EdgeKindId, Engine};
//!
//! const FOLLOWS: EdgeKindId = EdgeKindId(0);
//!
//! let mut b = DatabaseBuilder::new();
//! b.set_inverse(FOLLOWS);
//! b.edge(FOLLOWS, 100, 200); // edges are declared by user key
//! let db = b.build();
//!
//! let mut engine = Engine::open("my-graph-db").unwrap();
//! db.save(&mut engine).unwrap();
//! drop(engine);
//!
//! let engine = Engine::open("my-graph-db").unwrap();
//! let db = Database::load(&engine).unwrap();
//!
//! let node = db.lookup(100).expect("known key");
//! let friends: Vec<u64> = db
//!     .expand(node, FOLLOWS, Direction::Outgoing)
//!     .into_iter()
//!     .map(|id| db.key_of(id).unwrap())
//!     .collect();
//! assert_eq!(friends, vec![200]);
//! ```

mod database;

pub use database::{Database, DatabaseBuilder};
pub use grahh_adjacency::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId, NodeId};
pub use grahh_index::{LabelIndex, LabelIndexBuilder};
pub use grahh_lsm::{Engine, EngineConfig, Key, LsmError, Value};
