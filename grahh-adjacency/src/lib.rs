//! GRAHH-DB adjacency store: the topology/connectivity layer.
//!
//! This crate implements the **topology half** of the data model.
//!
//! At the bottom is [`ChunkedCsr`] (in `csr`), a single sorted neighbor column
//! (+ offsets) split into fixed-size, codec-compressed chunks. It answers,
//! for *one* edge kind, "which nodes does this node connect to?"
//!
//! On top is the [`AdjacencyStore`] registry (in `store`): one directional
//! CSR per edge kind, where [`AdjacencyStore::neighbors`] takes an
//! [`EdgeKindId`] and a [`Direction`] and resolves to a concrete CSR. This is
//! the expansion primitive behind "give me all `friends` of node 2" and,
//! by iterating, nested traversal.
//!
//! Both levels persist through the byte format in `grahh-persist`:
//! [`ChunkedCsr::serialize`] / [`ChunkedCsr::deserialize`] and
//! [`AdjacencyStore::serialize`] / [`AdjacencyStore::deserialize`] convert to
//! and from a `Vec<u8>`. The bytes describe themselves — fixed-width
//! little-endian fields, length-prefixed regions, chunk geometry stored as
//! data, compressed chunks copied verbatim — and the header's version field
//! only records which layout they are in. Loading validates structure *and*
//! every chunk payload up front, so [`PersistenceError`] is the only failure
//! path and nothing is checked twice.
//!
//! Scope notes (per the data model decisions):
//! - Edges are **directed**; a reversed (in) adjacency list is only built if
//!   a kind declares an inverse via [`AdjacencyStoreBuilder::set_inverse`].
//!   There is no automatic `followedBy`.
//! - Each kind names one target node kind; direction is a query-layer concern.
//! - Dependent properties (e.g. a synced follower count) are out of scope for
//!   the adjacency store itself.

mod csr;
mod store;

pub use csr::{ChunkedCsr, CsrBuilder, NodeId};
pub use grahh_persist::PersistenceError;
pub use store::{AdjacencyStore, AdjacencyStoreBuilder, Direction, EdgeKindId};
