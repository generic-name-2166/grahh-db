//! Chunked Compressed Sparse Row (CSR) adjacency store — **one per edge kind**.
//!
//! This is the heart of "index-free adjacency": a node's neighbors are found
//! by one array-index hop into a flat offset array, then reading a slice of a
//! flat neighbor-id column — no index *seek* between friends.
//!
//! The store is **scoped to a single edge kind**, so each entry is a bare
//! target id (not `(target, kind, direction)`); the kind is a property of the
//! store and `direction` is a query-layer concern. This keeps the neighbor
//! column a clean, compressible `NodeId` column.
//!
//! Internally the flat neighbor column is split into fixed-size **chunks**,
//! each compressed with a codec from [`grahh_codecs`]. Reading a node's
//! neighbors decodes only the chunk(s) covering that node's slice.
//!
//! The store is **immutable**: fields are stored in [`Box<[_]>`] (no spare
//! capacity). Construction happens in a [`CsrBuilder`], which consumes its
//! scratch `Vec`s when converting to the boxed form via [`CsrBuilder::build`].
//!
//! ```text
//! offsets[i]    : start of node i's slice in the flat neighbor column
//! node i's ids  : neighbors[ offsets[i] .. offsets[i+1] ]
//! ```
//! Equal consecutive offsets mean a node has no neighbors — no special case.

use std::{collections::BTreeMap, range::Range};

use grahh_codecs::{Codec, CoreCodec};

/// A dense internal node id: an index into the node store, also used to index
/// into this CSR's offset array. Dense (no gaps) by renumbering, so the offset
/// array stays compact and directly indexable.
pub type NodeId = u64;

/// Number of neighbor entries packed into one chunk.
///
/// A constant for the first implementation; later a per-store metadata value.
/// Chunks start on `CHUNK_SLOTS`-aligned flat positions, so a chunk's start is
/// derivable (`chunk_index * CHUNK_SLOTS`); only the final chunk may be
/// shorter than `CHUNK_SLOTS`.
pub const CHUNK_SLOTS: usize = 64;

/// One fixed-size slice of the neighbor column, compressed.
///
/// `len` is how many target ids this chunk holds; `packed` is its codec
/// encoded bytes. Because chunks start on `CHUNK_SLOTS`-aligned positions, the
/// *position* of a chunk is derived from its index; only length and bytes are
/// stored.
#[derive(Debug)]
struct Chunk {
    /// Number of target ids stored in this chunk.
    len: usize,
    /// Codec-encoded bytes for exactly these `len` ids.
    packed: Box<[u8]>,
}

/// Build a [`ChunkedCsr`] from raw, directed edges.
///
/// Accumulate edges with `add_edge`/`extend_edges`, then `build` to sort,
/// chunk, and compress into the immutable store. This is deliberately the
/// same shape a future compaction pass would use, so this builder doubles as
/// the prototype for the write/compaction half of the engine.
#[derive(Debug, Default)]
pub struct CsrBuilder {
    edges: Vec<(NodeId, NodeId)>,
}

impl CsrBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a builder that already owns a set of `(source, target)` edges.
    ///
    /// This takes ownership with **no copying** — useful when a consumer
    /// already has the edge list (e.g. during a grouped build) and wants to
    /// hand it to the CSR without an extra allocation.
    pub fn from_edges(edges: Vec<(NodeId, NodeId)>) -> Self {
        Self { edges }
    }

    /// Record a directed edge `source -> target` (internal ids).
    pub fn add_edge(&mut self, source: NodeId, target: NodeId) {
        self.edges.push((source, target));
    }

    /// Record many directed edges at once.
    pub fn extend_edges<I: IntoIterator<Item = (NodeId, NodeId)>>(&mut self, iter: I) {
        self.edges.extend(iter);
    }

    /// Consume the builder, producing the immutable, chunked, compressed CSR.
    ///
    /// Neighbors are grouped by source and sorted by target id: this makes the
    /// offset array dense and compact, makes the neighbor column easy to
    /// compress (and, later, binary-searchable), and gives deterministic
    /// output. Equal `(source, target)` pairs keep their relative order
    /// (stable sort).
    pub fn build(self) -> ChunkedCsr {
        let mut edges: Vec<(u64, u64)> = self.edges;
        edges.sort_by_key(|&(s, t)| (s, t));

        // Upper bound on source ids referenced. Valid source ids are 0..=max.
        let node_count = match edges.iter().map(|&(s, _)| s).max() {
            Some(max_source) => max_source as usize + 1,
            None => 0,
        };

        // Count neighbors per source, then build exclusive-prefix offsets.
        let mut counts: Vec<usize> = vec![0; node_count];
        for &(s, _) in &edges {
            counts[s as usize] += 1;
        }
        let mut offsets: Box<[usize]> = (0..=node_count).map(|_| 0).collect();
        for i in 0..node_count {
            offsets[i + 1] = offsets[i] + counts[i];
        }

        // The flat neighbor column. Edges are sorted by (source, target), so
        // pushing target ids in sorted order groups them correctly by source.
        let neighbors: Vec<NodeId> = edges.into_iter().map(|(_, t)| t).collect();

        ChunkedCsr::from_parts(offsets, neighbors)
    }
}

/// An immutable, chunked CSR neighbor store scoped to a single edge kind.
///
/// `offsets[i] .. offsets[i+1]` indexes the flat compressed neighbor column
/// and yields node `i`'s outgoing neighbors for this edge kind. Fields are
/// boxed slices because the store is immutable — there is never spare capacity.
#[derive(Debug)]
pub struct ChunkedCsr {
    /// `offsets[i]` = start of node `i`'s slice in the flat neighbor column.
    /// Length is `node_count + 1`. Equal consecutive offsets mean a node has
    /// no neighbors (handled with no special case).
    offsets: Box<[usize]>,
    /// The flat neighbor column, split into fixed-size compressed chunks.
    /// `neighbors[ci]` covers flat positions
    /// `[ ci*CHUNK_SLOTS .. ci*CHUNK_SLOTS + neighbors[ci].len )`.
    neighbors: Box<[Chunk]>,
}

impl ChunkedCsr {
    /// Build a store directly from a precomputed offset array and a flat,
    /// already-sorted neighbor column. Internal; used by `CsrBuilder::build`.
    fn from_parts(offsets: Box<[usize]>, neighbors: Vec<NodeId>) -> Self {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut pos: usize = 0;
        while pos < neighbors.len() {
            let end: usize = (pos + CHUNK_SLOTS).min(neighbors.len());
            let slice: &[u64] = &neighbors[pos..end];

            let mut packed: Vec<u8> = Vec::new();
            CoreCodec.encode(slice, &mut packed).unwrap();

            chunks.push(Chunk {
                len: slice.len(),
                packed: packed.into_boxed_slice(),
            });
            pos = end;
        }

        Self {
            offsets,
            neighbors: chunks.into_boxed_slice(),
        }
    }

    /// Number of distinct node ids indexed by this store.
    pub fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Total number of stored neighbor entries (edges) in this store.
    pub fn edge_count(&self) -> usize {
        self.offsets.last().copied().unwrap_or_default()
    }

    /// Total encoded size of this store in bytes: the offsets array plus every
    /// compressed chunk payload (each chunk includes its 1-byte format tag).
    pub fn encoded_size(&self) -> usize {
        let offsets_bytes = self.offsets.len() * size_of::<usize>();
        let chunks_bytes: usize = self.neighbors.iter().map(|c| c.packed.len()).sum();
        offsets_bytes + chunks_bytes
    }

    /// Per-format usage: how many chunks were compressed by each format and
    /// how many bytes they occupy. Reads the tag byte from each chunk.
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

    /// The slice range `[start, end)` of `node`'s neighbors in the flat
    /// column, or `None` if `node` is not indexed by this store.
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

    /// Number of outgoing neighbors of `node` for this edge kind (`O(1)`, no
    /// decode).
    pub fn degree(&self, node: NodeId) -> usize {
        self.offset_range(node)
            .map_or(0, |r: Range<usize>| r.end - r.start)
    }

    /// Outgoing neighbors of `node` for this edge kind, eager.
    ///
    /// Decodes only the chunk(s) covering the node's slice, appending into a
    /// single pre-sized buffer (no per-chunk temporary `Vec`s). Returns an
    /// empty vec for a node with no neighbors or an out-of-range id.
    pub fn neighbors(&self, node: NodeId) -> Vec<NodeId> {
        let Some(range) = self.offset_range(node) else {
            return Vec::new();
        };
        if range.is_empty() {
            return Vec::new();
        }

        let first_chunk: usize = range.start / CHUNK_SLOTS;
        let last_chunk: usize = (range.end - 1) / CHUNK_SLOTS;

        let mut out: Vec<NodeId> = Vec::with_capacity(range.end - range.start);
        // Reuse one decode buffer across chunks to avoid reallocating.
        let mut decoded: Vec<NodeId> = Vec::with_capacity(CHUNK_SLOTS);

        for ci in first_chunk..=last_chunk {
            let chunk: &Chunk = &self.neighbors[ci];
            let cstart: usize = ci * CHUNK_SLOTS;
            let cend: usize = cstart + chunk.len;
            let lo: usize = range.start.max(cstart);
            let hi: usize = range.end.min(cend);

            decoded.clear();
            CoreCodec.decode(&chunk.packed, &mut decoded).unwrap();

            out.extend_from_slice(&decoded[lo - cstart..hi - cstart]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_from(edges: &[(NodeId, NodeId)]) -> ChunkedCsr {
        let mut b = CsrBuilder::new();
        b.extend_edges(edges.iter().copied());
        b.build()
    }

    #[test]
    fn basic_adjacency() {
        // 4 nodes; 0->{1,2}, 1->{2}, 3->{0}
        let csr = build_from(&[(0, 1), (0, 2), (1, 2), (3, 0)]);
        assert_eq!(csr.node_count(), 4);
        assert_eq!(csr.edge_count(), 4);
        assert_eq!(csr.degree(0), 2);
        assert_eq!(csr.neighbors(0), vec![1, 2]);
        assert_eq!(csr.neighbors(1), vec![2]);
        // node 2 has no outgoing neighbors
        assert_eq!(csr.degree(2), 0);
        assert_eq!(csr.neighbors(2), vec![]);
        assert_eq!(csr.neighbors(3), vec![0]);
    }

    #[test]
    fn out_of_range_is_empty() {
        let csr = build_from(&[]);
        assert_eq!(csr.node_count(), 0);
        assert_eq!(csr.edge_count(), 0);
        assert_eq!(csr.degree(0), 0);
        assert_eq!(csr.neighbors(0), vec![]);
        assert_eq!(csr.neighbors(999), vec![]);
    }

    #[test]
    fn chunk_boundaries_are_seamless() {
        // More than one chunk's worth so the slice spans a boundary.
        let mut edges = Vec::new();
        for i in 1..=(CHUNK_SLOTS + 10) {
            edges.push((0, i as NodeId));
        }
        let csr = build_from(&edges);
        assert!(csr.edge_count() > CHUNK_SLOTS);
        let got = csr.neighbors(0);
        assert_eq!(got.len(), CHUNK_SLOTS + 10);
        assert_eq!(got.first().copied(), Some(1));
        assert_eq!(got.last().copied(), Some((CHUNK_SLOTS + 10) as NodeId));
    }

    #[test]
    fn targets_are_sorted() {
        // unsorted input -> sorted, dedup-free output
        let csr = build_from(&[(0, 3), (0, 1), (0, 2)]);
        assert_eq!(csr.neighbors(0), vec![1, 2, 3]);
    }

    #[test]
    fn many_nodes_each_with_a_few_neighbors() {
        let mut edges = Vec::new();
        for s in 0..50_u64 {
            for t in 0..3_u64 {
                edges.push((s, t + s + 1));
            }
        }
        let csr = build_from(&edges);
        assert_eq!(csr.node_count(), 50);
        assert_eq!(csr.edge_count(), 150);
        assert_eq!(csr.neighbors(0), vec![1, 2, 3]);
        assert_eq!(csr.degree(0), 3);
        assert_eq!(csr.neighbors(49), vec![50, 51, 52]);
    }
}
