//! GRAHH-DB adjacency store: chunked CSR per edge kind.
//!
//! This crate implements the **topology half** of the data model: for each
//! edge kind, a single [`ChunkedCsr`] answers "which nodes does this node
//! connect to, via edges of this kind?" The neighbors are stored as a flat,
//! sorted `u64` column, split into fixed-size chunks, each compressed by a
//! [`grahh_codecs`] codec.
//!
//! Scope notes (per the data model decisions):
//! - Edges are **directed**; a reversed adjacency list is only constructed if
//!   the schema explicitly declares an inverse edge kind. There is no
//!   automatic `followedBy`.
//! - Each store is **scoped to one edge kind**, so entries are bare target
//!   ids; edge *kind* and the query-time `direction` are handled above this
//!   crate.
//! - Dependent properties (e.g. a synced follower count) are deliberately
//!   out of scope for the adjacency store itself.

mod csr;

pub use csr::{CHUNK_SLOTS, ChunkedCsr, CsrBuilder, NodeId};
