//! The GRAHH-DB database: stores composed into one image.
//!
//! This crate is the engine's front door. It owns the *envelope* — the outer
//! container that says what lives in a database file and where — plus the
//! [`Database`] on top of it: identity, topology, and the primitives that stitch
//! them together.
//!
//! ```text
//! image : header + region directory
//!         region 1: label index        (user key <-> dense internal id)
//!         region 2: adjacency registry (topology per edge kind)
//! ```
//!
//! Everything is immutable and persisted whole: build with [`DatabaseBuilder`],
//! serialize to bytes, load with [`Database::deserialize`] — one strict
//! validation pass, then trust.
//!
//! ```no_run
//! use libgrahh::{Database, DatabaseBuilder, Direction, EdgeKindId};
//!
//! const FOLLOWS: EdgeKindId = EdgeKindId(0);
//!
//! let mut b = DatabaseBuilder::new();
//! b.set_inverse(FOLLOWS);
//! b.edge(FOLLOWS, 100, 200); // edges are declared by user key
//! let db = b.build();
//!
//! let mut bytes: Vec<u8> = Vec::new();
//! db.serialize(&mut bytes).unwrap();
//! let db = Database::deserialize(&bytes).unwrap();
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
mod image;

pub use database::{Database, DatabaseBuilder};
pub use grahh_adjacency::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId, NodeId};
pub use grahh_index::{LabelIndex, LabelIndexBuilder};
pub use grahh_persist::PersistenceError;
