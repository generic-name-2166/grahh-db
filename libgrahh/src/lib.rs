//! The GRAHH-DB embedded database: identity, adjacency, and properties on LSMT.
//!
//! `Database` is a connection-style API (open a path, read/write directly),
//! backed by [`grahh_lsm::Engine`]. Adjacency and properties share the same
//! LSMT list/cell storage: each `(node_id, kind_id)` maps to rows with replace
//! semantics. Adjacency rows encode `[target_id][edge_id]`; property rows are
//! typed variable-length blobs.
//!
//! ```no_run
//! use libgrahh::{Database, Direction, EdgeKindId, PropertyKindId, PropertyType, PropertyValue};
//!
//! const FOLLOWS: EdgeKindId = EdgeKindId(0);
//! let mut db = Database::open("my.grahh").unwrap();
//! db.set_inverse(FOLLOWS).unwrap();
//! db.add_edge(FOLLOWS, 100, 200).unwrap();
//! let n = db.lookup(100).unwrap();
//! let out = db.expand(n, FOLLOWS, Direction::Outgoing);
//! assert_eq!(out, vec![db.lookup(200).unwrap()]);
//!
//! let name = PropertyKindId(10_000);
//! db.set_property(n, name, &PropertyValue::String("alice".into()), &PropertyType::String).unwrap();
//! db.flush().unwrap();
//! ```

mod adjacency;
mod database;
mod identity;
pub mod properties;

pub use adjacency::{
    AdjacencyStore, AdjacencyStoreBuilder, ChunkedCsr, CsrBuilder, Direction, EdgeKindId,
};
pub use database::Database;
pub use grahh_lsm::{Engine, EngineConfig, Key, LsmError, Value};
pub use identity::{LabelIndex, LabelIndexBuilder, NodeId};
pub use properties::{
    Cardinality, PropertyDef, PropertyKindId, PropertyStore, PropertyType, PropertyValue,
};
