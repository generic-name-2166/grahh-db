//! Typed adjacency registry: the topology/connectivity layer above a single
//! [`ChunkedCsr`].
//!
//! A single [`ChunkedCsr`] answers "which nodes does this node connect to via
//! *one* edge kind." An [`AdjacencyStore`] answers the real query: "which
//! nodes does this node connect to via *edge kind K*, in direction D." It is
//! the thing that makes a `friends` lookup resolve to a concrete CSR, and it
//! is the primitive underlying nested-traversal.
//!
//! Design:
//! - One universal node id namespace across all kinds (they share the node
//!   universe).
//! - One [`DirectionalCsr`] per edge kind: an always-present out-CSR plus an
//!   *optional* in-CSR, present only if that kind declared an inverse.
//! - `node_count` is the maximum over all kinds, so a node present only as a
//!   target in some kind's store is still addressable.

use crate::csr::{ChunkedCsr, CsrBuilder, NodeId};

/// Identifier for an edge kind. Opaque to this crate; assigned by the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKindId(pub u32);

/// Direction of traversal through an edge kind's adjacency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// src -> target (forward edges).
    Outgoing,
    /// target -> src (reverse edges; present only if an inverse was declared).
    Incoming,
}

/// One edge kind's connectivity: forward adjacency, plus reverse if declared.
#[derive(Debug)]
struct DirectionalCsr {
    out: ChunkedCsr,
    in_: Option<ChunkedCsr>,
}

/// The typed per-kind adjacency registry.
///
/// `neighbors(node, kind, dir)` is the headline expansion primitive that
/// underlies "give me all `friends` of node 2" and the nested-traversal path.
#[derive(Debug)]
pub struct AdjacencyStore {
    /// One directional CSR per edge kind, as declared to the builder.
    by_kind: Box<[(EdgeKindId, DirectionalCsr)]>,
}

impl AdjacencyStore {
    /// The CSR handling `(kind, dir)`, if any.
    fn csr_for(&self, kind: EdgeKindId, dir: Direction) -> Option<&ChunkedCsr> {
        let (_, d) = self.by_kind.iter().find(|(k, _)| *k == kind)?;
        match dir {
            Direction::Outgoing => Some(&d.out),
            Direction::Incoming => d.in_.as_ref(),
        }
    }

    /// Neighbors of `node` via edge kind `kind` in direction `dir`.
    ///
    /// Returns an empty vec if the kind is unknown, the node is out of range,
    /// or the requested direction was never declared.
    pub fn neighbors(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<NodeId> {
        self.csr_for(kind, dir)
            .map_or_else(Vec::new, |csr: &ChunkedCsr| csr.neighbors(node))
    }

    /// Degree of `node` via `kind` in `dir` (O(1); 0 if unknown/undeclared).
    pub fn degree(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> usize {
        self.csr_for(kind, dir)
            .map_or_default(|csr: &ChunkedCsr| csr.degree(node))
    }

    /// Total edges across all declared kinds/directions.
    pub fn edge_count(&self) -> usize {
        self.by_kind
            .iter()
            .map(|(_, d)| {
                d.out.edge_count()
                    + d.in_
                        .as_ref()
                        .map_or_default(|c: &ChunkedCsr| c.edge_count())
            })
            .sum()
    }
}

/// Build a single forward CSR from an *owned* edge list, moving it into the
/// builder so no copy/reallocation happens.
fn build_forward(edges: Vec<(NodeId, NodeId)>) -> ChunkedCsr {
    CsrBuilder::from_edges(edges).build()
}

/// Build an [`AdjacencyStore`] from per-kind raw edges.
///
/// `add_edge(kind, src, target)` records a *forward* edge. Call
/// [`Self::set_inverse`] on a kind to also build its reverse (in-)CSR; reverse
/// storage is only paid for kinds that opt in. The store is produced once via
/// [`Self::build`].
#[derive(Debug, Default)]
pub struct AdjacencyStoreBuilder {
    /// forward edges per kind: (kind, source, target)
    out_edges: Vec<(EdgeKindId, NodeId, NodeId)>,
    /// kinds that should also get a reverse (in-)CSR
    inverse_kinds: Vec<EdgeKindId>,
}

impl AdjacencyStoreBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a forward edge `source -> target` of edge kind `kind`.
    pub fn add_edge(&mut self, kind: EdgeKindId, source: NodeId, target: NodeId) {
        self.out_edges.push((kind, source, target));
    }

    /// Declare that `kind` should also expose an incoming (reverse) CSR.
    pub fn set_inverse(&mut self, kind: EdgeKindId) {
        if !self.inverse_kinds.contains(&kind) {
            self.inverse_kinds.push(kind);
        }
    }

    /// Consume the builder into an immutable [`AdjacencyStore`].
    ///
    /// Groups forward edges by kind (owning the data — no copying), builds an
    /// out-CSR per kind, and an in-CSR for each inverse kind (from the same
    /// edges, reversed). `node_count` is the max node id referenced as a
    /// source across all kinds.
    pub fn build(self) -> AdjacencyStore {
        let Self {
            out_edges,
            inverse_kinds,
        } = self;

        // Group forward edges by kind, keeping insertion order of first
        // occurrence. Each group owns its edges (moved, not copied).
        let mut kinds: Vec<EdgeKindId> = Vec::new();
        let mut grouped: Vec<Vec<(NodeId, NodeId)>> = Vec::new();
        for (kind, src, tgt) in out_edges {
            match kinds.iter().position(|&k| k == kind) {
                Some(i) => grouped[i].push((src, tgt)),
                None => {
                    kinds.push(kind);
                    grouped.push(vec![(src, tgt)]);
                }
            }
        }

        // Build a DirectionalCsr per kind; edges are moved into the CSR (via
        // `from_edges`) so nothing is copied.
        let by_kind: Box<[(EdgeKindId, DirectionalCsr)]> = kinds
            .into_iter()
            .zip(grouped)
            .map(|(kind, edges)| {
                let wants_inverse: bool = inverse_kinds.contains(&kind);
                let in_: Option<ChunkedCsr> = if wants_inverse {
                    let rev: Vec<(NodeId, NodeId)> = edges.iter().map(|&(s, t)| (t, s)).collect();
                    Some(build_forward(rev))
                } else {
                    None
                };
                (
                    kind,
                    DirectionalCsr {
                        out: build_forward(edges),
                        in_,
                    },
                )
            })
            .collect();

        AdjacencyStore { by_kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLLOWS: EdgeKindId = EdgeKindId(0);
    const BLOCKS: EdgeKindId = EdgeKindId(1);

    #[test]
    fn expands_by_kind_and_direction() {
        // follows: 0->1, 0->2, 1->2   (inverse declared)
        // blocks:  2->0               (no inverse declared)
        let mut b = AdjacencyStoreBuilder::new();
        b.set_inverse(FOLLOWS);
        b.add_edge(FOLLOWS, 0, 1);
        b.add_edge(FOLLOWS, 0, 2);
        b.add_edge(FOLLOWS, 1, 2);
        b.add_edge(BLOCKS, 2, 0);
        let store = b.build();

        assert_eq!(store.neighbors(0, FOLLOWS, Direction::Outgoing), vec![1, 2]);
        assert_eq!(store.neighbors(1, FOLLOWS, Direction::Outgoing), vec![2]);
        // incoming since inverse declared
        assert_eq!(store.neighbors(2, FOLLOWS, Direction::Incoming), vec![0, 1]);
        assert_eq!(store.neighbors(0, FOLLOWS, Direction::Incoming), vec![]);
        assert_eq!(store.neighbors(2, BLOCKS, Direction::Outgoing), vec![0]);
        // no inverse declared -> incoming empty
        assert_eq!(store.neighbors(0, BLOCKS, Direction::Incoming), vec![]);
    }

    #[test]
    fn unknown_kind_and_out_of_range_are_empty() {
        let mut b = AdjacencyStoreBuilder::new();
        b.add_edge(FOLLOWS, 0, 1);
        let store = b.build();

        assert_eq!(store.neighbors(0, BLOCKS, Direction::Outgoing), vec![]);
        assert_eq!(store.neighbors(999, FOLLOWS, Direction::Outgoing), vec![]);
        assert_eq!(store.neighbors(1, FOLLOWS, Direction::Incoming), vec![]);
    }

    #[test]
    fn inverse_is_optional_storage() {
        let mut b = AdjacencyStoreBuilder::new();
        b.add_edge(FOLLOWS, 0, 1); // no inverse
        let store = b.build();
        assert_eq!(store.edge_count(), 1);
        assert_eq!(store.neighbors(1, FOLLOWS, Direction::Incoming), vec![]);
    }

    #[test]
    fn node_count_tracks_max_across_kinds() {
        let mut b = AdjacencyStoreBuilder::new();
        b.add_edge(FOLLOWS, 0, 1);
        b.add_edge(BLOCKS, 5, 0);
        let store = b.build();
        assert_eq!(store.edge_count(), 2);
    }

    #[test]
    fn degree_reflects_direction() {
        let mut b = AdjacencyStoreBuilder::new();
        b.set_inverse(FOLLOWS);
        b.add_edge(FOLLOWS, 0, 1);
        b.add_edge(FOLLOWS, 0, 2);
        b.add_edge(FOLLOWS, 1, 2);
        let store = b.build();
        assert_eq!(store.degree(0, FOLLOWS, Direction::Outgoing), 2);
        assert_eq!(store.degree(2, FOLLOWS, Direction::Incoming), 2);
        assert_eq!(store.degree(1, FOLLOWS, Direction::Outgoing), 1);
    }
}
