//! In-memory adjacency structures over the LSM's list storage model.
//!
//! The durable source of truth for adjacency is the LSM engine: for each
//! `(node_id, kind_id)` key it stores a `Many` list of `(target_id, edge_id)`
//! rows (replace semantics per key/tablet generation). This module provides a
//! query-optimized in-memory view (chunked compressed neighbor columns) and the
//! same builder API used by [`crate::Database`].
//!
//! Properties use the same LSM list/cell storage primitives; property-layer
//! APIs live alongside adjacency in `libgrahh`.

use std::{collections::BTreeMap, range::Range};

use grahh_codecs::{Codec, CoreCodec};

use crate::identity::NodeId;

/// Identifier for an edge kind. Opaque to storage; assigned by schema/user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKindId(pub u32);

/// Direction of traversal through an edge kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Debug)]
struct Chunk {
    len: usize,
    packed: Box<[u8]>,
}

#[derive(Debug, Default)]
pub struct CsrBuilder {
    edges: Vec<(NodeId, NodeId)>,
    chunk_slots: usize,
}

impl CsrBuilder {
    pub fn new() -> Self {
        Self {
            edges: Vec::new(),
            chunk_slots: 64,
        }
    }

    pub fn from_edges(edges: Vec<(NodeId, NodeId)>) -> Self {
        Self {
            edges,
            chunk_slots: 64,
        }
    }

    pub fn add_edge(&mut self, source: NodeId, target: NodeId) {
        self.edges.push((source, target));
    }

    pub fn extend_edges<I: IntoIterator<Item = (NodeId, NodeId)>>(&mut self, iter: I) {
        self.edges.extend(iter);
    }

    pub fn set_chunk_slots(&mut self, chunk_slots: usize) {
        assert!(chunk_slots > 0, "chunk geometry must be at least 1");
        self.chunk_slots = chunk_slots;
    }

    pub fn build(self) -> ChunkedCsr {
        let chunk_slots = self.chunk_slots;
        let mut edges = self.edges;
        edges.sort_by_key(|&(s, t)| (s, t));

        let node_count = match edges.iter().map(|&(s, _)| s).max() {
            Some(max_source) => max_source as usize + 1,
            None => 0,
        };

        let mut counts = vec![0_usize; node_count];
        for &(s, _) in &edges {
            counts[s as usize] += 1;
        }
        let mut offsets: Box<[usize]> = (0..=node_count).map(|_| 0).collect();
        for i in 0..node_count {
            offsets[i + 1] = offsets[i] + counts[i];
        }

        let neighbors: Vec<NodeId> = edges.into_iter().map(|(_, t)| t).collect();
        ChunkedCsr::from_parts(chunk_slots, offsets, neighbors)
    }
}

#[derive(Debug)]
pub struct ChunkedCsr {
    chunk_slots: usize,
    offsets: Box<[usize]>,
    neighbors: Box<[Chunk]>,
    id_span: usize,
}

impl ChunkedCsr {
    fn from_parts(chunk_slots: usize, offsets: Box<[usize]>, neighbors: Vec<NodeId>) -> Self {
        let mut chunks = Vec::new();
        let mut pos = 0_usize;
        while pos < neighbors.len() {
            let end = (pos + chunk_slots).min(neighbors.len());
            let slice = &neighbors[pos..end];
            let mut packed = Vec::new();
            CoreCodec.encode(slice, &mut packed).unwrap();
            chunks.push(Chunk {
                len: slice.len(),
                packed: packed.into_boxed_slice(),
            });
            pos = end;
        }

        let node_count = offsets.len().saturating_sub(1);
        let id_span = match neighbors.iter().copied().max() {
            Some(id) => {
                let span = (node_count as u64).max(id.saturating_add(1));
                usize::try_from(span).unwrap_or(usize::MAX)
            }
            None => node_count,
        };

        Self {
            chunk_slots,
            offsets,
            neighbors: chunks.into_boxed_slice(),
            id_span,
        }
    }

    pub fn chunk_slots(&self) -> usize {
        self.chunk_slots
    }

    pub fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn edge_count(&self) -> usize {
        self.offsets.last().copied().unwrap_or_default()
    }

    pub fn id_span(&self) -> usize {
        self.id_span
    }

    pub fn encoded_size(&self) -> usize {
        let offsets_bytes = self.offsets.len() * size_of::<usize>();
        let chunks_bytes: usize = self.neighbors.iter().map(|c| c.packed.len()).sum();
        offsets_bytes + chunks_bytes
    }

    pub fn format_usage(&self) -> Vec<(&'static str, usize, usize)> {
        let mut usage: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
        for c in self.neighbors.iter() {
            let name = grahh_codecs::format_name(&c.packed).unwrap_or("?");
            let e = usage.entry(name).or_insert((0, 0));
            e.0 += 1;
            e.1 += c.packed.len();
        }
        usage.into_iter().map(|(k, (n, b))| (k, n, b)).collect()
    }

    fn offset_range(&self, node: NodeId) -> Option<Range<usize>> {
        let idx = node as usize;
        if idx >= self.node_count() {
            return None;
        }
        Some(Range {
            start: self.offsets[idx],
            end: self.offsets[idx + 1],
        })
    }

    pub fn degree(&self, node: NodeId) -> usize {
        self.offset_range(node)
            .map_or(0, |r: Range<usize>| r.end - r.start)
    }

    pub fn neighbors(&self, node: NodeId) -> Vec<NodeId> {
        let Some(range) = self.offset_range(node) else {
            return Vec::new();
        };
        if range.is_empty() {
            return Vec::new();
        }

        let first_chunk = range.start / self.chunk_slots;
        let last_chunk = (range.end - 1) / self.chunk_slots;

        let mut out = Vec::with_capacity(range.end - range.start);
        let mut decoded = Vec::with_capacity(self.chunk_slots);
        for ci in first_chunk..=last_chunk {
            let chunk = &self.neighbors[ci];
            let cstart = ci * self.chunk_slots;
            let cend = cstart + chunk.len;
            let lo = range.start.max(cstart);
            let hi = range.end.min(cend);
            decoded.clear();
            CoreCodec
                .decode(&chunk.packed, &mut decoded)
                .expect("chunk payloads are produced by CoreCodec");
            out.extend_from_slice(&decoded[lo - cstart..hi - cstart]);
        }
        out
    }

    pub fn edges(&self) -> impl Iterator<Item = (NodeId, NodeId)> + '_ {
        (0..self.node_count() as NodeId).flat_map(move |source| {
            self.neighbors(source)
                .into_iter()
                .map(move |target| (source, target))
        })
    }
}

#[derive(Debug)]
struct DirectionalCsr {
    out: ChunkedCsr,
    in_: Option<ChunkedCsr>,
}

#[derive(Debug, Default)]
pub struct AdjacencyStore {
    by_kind: Box<[(EdgeKindId, DirectionalCsr)]>,
}

impl AdjacencyStore {
    fn csr_for(&self, kind: EdgeKindId, dir: Direction) -> Option<&ChunkedCsr> {
        let (_, d) = self.by_kind.iter().find(|(k, _)| *k == kind)?;
        match dir {
            Direction::Outgoing => Some(&d.out),
            Direction::Incoming => d.in_.as_ref(),
        }
    }

    pub fn neighbors(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> Vec<NodeId> {
        self.csr_for(kind, dir)
            .map_or_else(Vec::new, |csr| csr.neighbors(node))
    }

    pub fn degree(&self, node: NodeId, kind: EdgeKindId, dir: Direction) -> usize {
        self.csr_for(kind, dir).map_or(0, |csr| csr.degree(node))
    }

    pub fn kinds(&self) -> impl Iterator<Item = EdgeKindId> + '_ {
        self.by_kind.iter().map(|(kind, _)| *kind)
    }

    pub fn has_inverse(&self, kind: EdgeKindId) -> bool {
        self.by_kind
            .iter()
            .find(|(seen, _)| *seen == kind)
            .is_some_and(|(_, directional)| directional.in_.is_some())
    }

    pub fn out_edges(&self, kind: EdgeKindId) -> Vec<(NodeId, NodeId)> {
        self.csr_for(kind, Direction::Outgoing)
            .map(|csr| csr.edges().collect())
            .unwrap_or_default()
    }

    pub fn id_span(&self) -> usize {
        self.by_kind
            .iter()
            .map(|(_, d)| {
                d.in_
                    .as_ref()
                    .map_or(0, |c| c.id_span())
                    .max(d.out.id_span())
            })
            .max()
            .unwrap_or_default()
    }

    pub fn edge_count(&self) -> usize {
        self.by_kind
            .iter()
            .map(|(_, d)| d.out.edge_count() + d.in_.as_ref().map_or(0, |c| c.edge_count()))
            .sum()
    }
}

#[derive(Debug, Default)]
pub struct AdjacencyStoreBuilder {
    out_edges: Vec<(EdgeKindId, NodeId, NodeId)>,
    inverse_kinds: Vec<EdgeKindId>,
}

impl AdjacencyStoreBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&mut self, kind: EdgeKindId, source: NodeId, target: NodeId) {
        self.out_edges.push((kind, source, target));
    }

    pub fn set_inverse(&mut self, kind: EdgeKindId) {
        if !self.inverse_kinds.contains(&kind) {
            self.inverse_kinds.push(kind);
        }
    }

    pub fn build(self) -> AdjacencyStore {
        let Self {
            out_edges,
            inverse_kinds,
        } = self;

        let mut kinds = Vec::<EdgeKindId>::new();
        let mut grouped = Vec::<Vec<(NodeId, NodeId)>>::new();
        for (kind, src, tgt) in out_edges {
            match kinds.iter().position(|&k| k == kind) {
                Some(i) => grouped[i].push((src, tgt)),
                None => {
                    kinds.push(kind);
                    grouped.push(vec![(src, tgt)]);
                }
            }
        }

        let by_kind = kinds
            .into_iter()
            .zip(grouped)
            .map(|(kind, edges)| {
                let wants_inverse = inverse_kinds.contains(&kind);
                let in_ = if wants_inverse {
                    let rev = edges.iter().map(|&(s, t)| (t, s)).collect();
                    Some(CsrBuilder::from_edges(rev).build())
                } else {
                    None
                };
                (
                    kind,
                    DirectionalCsr {
                        out: CsrBuilder::from_edges(edges).build(),
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
        let mut b = AdjacencyStoreBuilder::new();
        b.set_inverse(FOLLOWS);
        b.add_edge(FOLLOWS, 0, 1);
        b.add_edge(FOLLOWS, 0, 2);
        b.add_edge(FOLLOWS, 1, 2);
        b.add_edge(BLOCKS, 2, 0);
        let store = b.build();

        assert_eq!(store.neighbors(0, FOLLOWS, Direction::Outgoing), vec![1, 2]);
        assert_eq!(store.neighbors(2, FOLLOWS, Direction::Incoming), vec![0, 1]);
        assert_eq!(store.neighbors(0, BLOCKS, Direction::Incoming), vec![]);
        assert_eq!(store.degree(0, FOLLOWS, Direction::Outgoing), 2);
    }
}
